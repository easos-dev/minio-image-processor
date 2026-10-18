//! 调用原生编码子进程执行处理串，并把子进程的结果整理成 OSS 同格式的应答正文。
//!
//! 编解码全部交给一次性 C 子进程（native/worker.c，libvips 与其原生 codec）。本模块负责：识别源图格式、
//! 把规范化的 [`Step`] 写成子进程参数、按管道逐段执行、把退出码映射成 OSS 错误、把 info / average-hue
//! 的原始数据排成 OSS 的 JSON 版式。子进程只拿到私有临时文件路径与已校验的标量，拿不到 URL、Host 或签名。

use crate::{
    error::{OssError, Result},
    process::{Format, Op, Output, Step},
};
use axum::{body::Bytes, http::StatusCode};
use std::{
    collections::BTreeMap,
    io::Write,
    process::{Command, Stdio},
};

/// OSS 图片处理的源图大小上限：20 MB（OSS 文档“源图文件大小不能超过 20 MB”，按 MiB 计）。
pub const MAX_SOURCE_BYTES: usize = 20 * 1024 * 1024;

/// 一次处理的最终产物。
#[derive(Clone, Debug)]
pub struct Product {
    /// 响应正文；`Bytes` 让缓存命中时只增引用计数，不复制大图。
    pub bytes: Bytes,
    /// 响应 Content-Type。
    pub content_type: &'static str,
    /// 输出图片宽（像素）；JSON 产物为 0。只用于日志。
    pub width: u32,
    /// 输出图片高（单帧，像素）；JSON 产物为 0。只用于日志。
    pub height: u32,
}

/// 只认明确的栅格魔数，不信任上游 Content-Type；SVG/PDF/脚本等一律不进通用解码器。
pub fn source_format(b: &[u8]) -> Option<Format> {
    if b.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(Format::Jpg)
    } else if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(Format::Png)
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some(Format::Gif)
    } else if b.starts_with(b"BM") {
        Some(Format::Bmp)
    } else if b.starts_with(b"II*\0")
        || b.starts_with(b"MM\0*")
        || b.starts_with(b"II+\0")
        || b.starts_with(b"MM\0+")
    {
        Some(Format::Tiff)
    } else if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        Some(Format::Webp)
    } else if b.get(4..8) == Some(b"ftyp") {
        match b.get(8..12)? {
            b"avif" | b"avis" => Some(Format::Avif),
            b"heic" | b"heix" | b"hevc" | b"hevx" | b"mif1" | b"msf1" => Some(Format::Heic),
            _ => None,
        }
    } else {
        None
    }
}

/// 一段处理在子进程参数里的写法（不含输入/输出路径与源格式）。
///
/// 这也是缓存键里的“规范化处理串”：同一语义的不同写法解析后得到相同的参数序列。
pub fn step_args(step: &Step) -> Vec<String> {
    let mut args = vec![
        u8::from(step.auto_orient).to_string(),
        match step.output {
            Output::Image => "image",
            Output::Info => "info",
            Output::AverageHue => "hue",
        }
        .to_owned(),
        step.format.map_or("source", Format::name).to_owned(),
        match step.quality {
            None => "none",
            Some(q) if q.absolute => "abs",
            Some(_) => "rel",
        }
        .to_owned(),
        step.quality.map_or(0, |q| q.value).to_string(),
        u8::from(step.interlace).to_string(),
    ];
    for op in &step.ops {
        match op {
            Op::Resize(r) => args.extend([
                "resize".into(),
                r.mode.name().into(),
                r.w.to_string(),
                r.h.to_string(),
                r.l.to_string(),
                r.s.to_string(),
                r.p.to_string(),
                u8::from(r.limit).to_string(),
                format!("{:02X}{:02X}{:02X}", r.color[0], r.color[1], r.color[2]),
            ]),
            Op::Crop(c) => args.extend([
                "crop".into(),
                c.w.to_string(),
                c.h.to_string(),
                c.x.to_string(),
                c.y.to_string(),
                c.gravity.name().into(),
            ]),
            Op::Rotate(angle) => args.extend(["rotate".into(), angle.to_string()]),
            Op::Circle(r) => args.extend(["circle".into(), r.to_string()]),
            Op::RoundedCorners(r) => args.extend(["rounded".into(), r.to_string()]),
            Op::IndexCrop {
                vertical,
                size,
                index,
            } => args.extend([
                "indexcrop".into(),
                if *vertical { "y" } else { "x" }.into(),
                size.to_string(),
                index.to_string(),
            ]),
            Op::Blur { radius, sigma } => {
                args.extend(["blur".into(), radius.to_string(), sigma.to_string()])
            }
            Op::Bright(v) => args.extend(["bright".into(), v.to_string()]),
            Op::Contrast(v) => args.extend(["contrast".into(), v.to_string()]),
            Op::Sharpen(v) => args.extend(["sharpen".into(), v.to_string()]),
        }
    }
    args
}

/// 按管道顺序执行全部段：前一段的编码结果作为后一段的输入，与 OSS 管道语义一致。
/// 产出 JSON 的段（info / average-hue）总是最后一段。
pub fn run(input: Vec<u8>, steps: &[Step]) -> Result<Product> {
    let mut data = input;
    let mut last = None;
    for step in steps {
        let source = source_format(&data).ok_or_else(OssError::unsupported_format)?;
        match run_step(&data, source, step)? {
            Done::Image {
                bytes,
                format,
                width,
                height,
            } => {
                data = bytes;
                last = Some((format, width, height));
            }
            Done::Json(bytes) => {
                return Ok(Product {
                    bytes: bytes.into(),
                    content_type: "application/json",
                    width: 0,
                    height: 0,
                });
            }
        }
    }
    let (format, width, height) = last.ok_or_else(OssError::worker_failed)?;
    Ok(Product {
        bytes: data.into(),
        content_type: format.mime(),
        width,
        height,
    })
}

/// 一段处理的结果。
enum Done {
    /// 编码后的图片，可作为下一段管道的输入。
    Image {
        /// 编码结果。
        bytes: Vec<u8>,
        /// 实际输出格式（gif 对非 GIF 源会落回源格式）。
        format: Format,
        /// 输出宽（像素）。
        width: u32,
        /// 输出单帧高（像素）。
        height: u32,
    },
    /// info / average-hue 的 JSON 正文。
    Json(Vec<u8>),
}

/// 执行一段：写临时输入、跑子进程、按退出码映射错误、读回输出。
///
/// 子进程不经 shell，stderr 丢弃（codec 可能打印不受信的元数据）。调用方在阻塞线程里持有并发名额直到
/// 这里返回，客户端取消不能突破资源上限。
fn run_step(input: &[u8], source: Format, step: &Step) -> Result<Done> {
    let directory = tempfile::tempdir().map_err(|_| OssError::worker_failed())?;
    let input_path = directory.path().join(format!("input.{}", source.name()));
    let output_path = directory.path().join("output");
    std::fs::File::create(&input_path)
        .and_then(|mut f| f.write_all(input))
        .map_err(|_| OssError::worker_failed())?;
    let worker = std::env::var_os("IMAGE_PROCESSOR_WORKER")
        .unwrap_or_else(|| "/usr/local/bin/image-worker".into());
    let result = Command::new(worker)
        .arg(&input_path)
        .arg(&output_path)
        .arg(source.name())
        .args(step_args(step))
        // libvips/ImageMagick 默认按宿主机核数开线程；并发由网关的名额统一控制，子进程只用单线程。
        .env("VIPS_CONCURRENCY", "1")
        .env("MAGICK_THREAD_LIMIT", "1")
        // 随机访问解码超过 libvips 默认 100 MB 时会改写临时文件，撞上子进程 80 MiB 的输出文件上限；
        // 改为在内存里解码，仍受 768 MiB 地址空间上限约束。
        .env("VIPS_DISC_THRESHOLD", "512m")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|_| OssError::worker_failed())?;
    if !result.status.success() {
        return Err(exit_error(result.status.code(), source));
    }
    let stdout = String::from_utf8(result.stdout).map_err(|_| OssError::worker_failed())?;
    let fields: Vec<&str> = stdout.split_whitespace().collect();
    let read_output = || std::fs::read(&output_path).map_err(|_| OssError::worker_failed());
    match fields.as_slice() {
        ["image", width, height, format] => {
            let format = [
                Format::Jpg,
                Format::Png,
                Format::Webp,
                Format::Bmp,
                Format::Gif,
                Format::Tiff,
                Format::Heic,
                Format::Avif,
            ]
            .into_iter()
            .find(|f| f.name() == *format)
            .ok_or_else(OssError::worker_failed)?;
            let bytes = read_output()?;
            if bytes.is_empty() {
                return Err(OssError::worker_failed());
            }
            Ok(Done::Image {
                bytes,
                format,
                width: width.parse().map_err(|_| OssError::worker_failed())?,
                height: height.parse().map_err(|_| OssError::worker_failed())?,
            })
        }
        ["info"] => {
            let raw = String::from_utf8(read_output()?).map_err(|_| OssError::worker_failed())?;
            Ok(Done::Json(info_json(&raw, input.len(), source)))
        }
        ["hue", r, g, b] => {
            let channel = |v: &str| v.parse::<u8>().map_err(|_| OssError::worker_failed());
            Ok(Done::Json(
                format!(
                    "{{\"RGB\": \"0x{:02x}{:02x}{:02x}\"}}",
                    channel(r)?,
                    channel(g)?,
                    channel(b)?
                )
                .into_bytes(),
            ))
        }
        _ => Err(OssError::worker_failed()),
    }
}

/// 子进程退出码到 OSS 错误的映射。退出码定义见 native/worker.c 开头。
///
/// 源图解码失败、超限的 EC 按 OSS 错误中心逐格式给出；OSS 没公开 Code/Message 原文的情形，
/// Code 取 OSS 错误码表里语义对应的一项（docs/protocol.md 列出了这些未经实测确认的项）。
fn exit_error(code: Option<i32>, source: Format) -> OssError {
    match code {
        Some(4) => match source {
            Format::Webp => OssError::bad(
                "BadWebPImage",
                "The webp image file may be damaged.",
                Some("0040-00000010"),
            ),
            Format::Heic => OssError::bad(
                "BadRequest",
                "The image file may be damaged.",
                Some("0040-00000020"),
            ),
            Format::Avif => OssError::bad(
                "BadRequest",
                "The image file may be damaged.",
                Some("0040-00000021"),
            ),
            _ => OssError::bad(
                "BadRequest",
                "The image file may be damaged.",
                Some("0040-00000002"),
            ),
        },
        Some(5) => OssError::bad(
            "ImageTooLarge",
            "The width or height of the source image exceeds the limit.",
            Some(match source {
                Format::Jpg => "0040-00000019",
                Format::Bmp => "0040-00000017",
                Format::Tiff => "0040-00000018",
                Format::Heic => "0040-00000013",
                Format::Avif => "0040-00000014",
                _ => "0040-00000009",
            }),
        ),
        Some(12) => OssError::bad(
            "ImageTooLarge",
            "The total pixels of the source image exceed the limit.",
            Some("0040-00000012"),
        ),
        Some(6) => OssError::bad(
            "BadRequest",
            "The heic image with alpha channel can not be saved as jpg.",
            None,
        ),
        // FAQ 原文的 Message。
        Some(7) => OssError::bad(
            "ImageTooLarge",
            "Picture exceed the maximum allowable rotation range.",
            Some("0040-00000006"),
        ),
        Some(8) => OssError::bad(
            "BadRequest",
            "The resolution of the destination image exceeds the limit.",
            Some("0040-00000007"),
        ),
        Some(9) => OssError::bad(
            "BadRequest",
            "The side length of the webp image exceeds the limit.",
            Some("0040-00000011"),
        ),
        Some(10) => OssError::bad(
            "BadRequest",
            "The resolution of the heic or avif image exceeds the limit.",
            Some("0040-00000014"),
        ),
        // OSS 实测：没有 EC。
        Some(11) => OssError::bad(
            "CutEdgeExceedRange",
            "Advance cut's position is out of image.",
            None,
        ),
        // 被内核按内存/CPU/墙钟/输出文件上限终止：图片复杂度超出单次处理预算，重试也不会成功。
        None => OssError::new(
            StatusCode::BAD_REQUEST,
            "ImageTooLarge",
            "Image processing exceeds the memory or time limit.",
            None,
            "processing_limit",
        ),
        _ => OssError::worker_failed(),
    }
}

/// 把子进程写出的原始字段排成 OSS `info` 的 JSON：键按字母序，四空格缩进，每项
/// `{"value": "…"}`，最后一项后直接接 `}}`、不换行（与 OSS 实测应答逐字节同版式）。
///
/// 子进程输出每行一项，制表符分隔：`width`/`height`/`frames`、`resunit`/`xres`/`yres`
/// （libvips 的分辨率，单位像素每毫米）、`exif <IFD> <标签名> <libvips 文本>`。
fn info_json(raw: &str, file_size: usize, source: Format) -> Vec<u8> {
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    let (mut unit, mut xres, mut yres) = (None, None, None);
    for line in raw.lines() {
        let parts: Vec<&str> = line.splitn(4, '\t').collect();
        match parts.as_slice() {
            ["width", v] => {
                fields.insert("ImageWidth".into(), (*v).into());
            }
            ["height", v] => {
                fields.insert("ImageHeight".into(), (*v).into());
            }
            ["frames", v] => {
                fields.insert("FrameCount".into(), (*v).into());
            }
            ["resunit", v] => unit = Some(*v),
            ["xres", v] => xres = v.parse::<f64>().ok(),
            ["yres", v] => yres = v.parse::<f64>().ok(),
            // OSS 不返回 PNG 的 EXIF。子进程先输出 IFD0 再输出其他 IFD，同名标签保留先出现的。
            // 核心尺寸字段以实际解码结果为准，EXIF 里的同名 TIFF 标签不覆盖它们。
            ["exif", _ifd, name, value]
                if source != Format::Png
                    && !matches!(*name, "ImageWidth" | "ImageHeight" | "FrameCount") =>
            {
                fields
                    .entry((*name).into())
                    .or_insert_with(|| exif_value(value).into());
            }
            _ => {}
        }
    }
    fields.insert("FileSize".into(), file_size.to_string());
    fields.insert("Format".into(), source.name().into());
    // 只有图片自带分辨率信息时才返回。OSS 实测 EXIF 写成 96000/1000 时仍显示 96/1，即分辨率按解码
    // 结果给出、不照搬 EXIF 的原始分数，所以这三项覆盖 EXIF 里的同名字段。
    if let (Some(unit), Some(x), Some(y)) = (unit, xres, yres) {
        // libvips 内部统一为像素每毫米，OSS 按原单位显示 “数值/1”。
        let (code, per_mm) = match unit {
            "cm" => ("3", 10.0),
            _ => ("2", 25.4),
        };
        fields.insert("ResolutionUnit".into(), code.into());
        fields.insert(
            "XResolution".into(),
            format!("{}/1", (x * per_mm).round() as i64),
        );
        fields.insert(
            "YResolution".into(),
            format!("{}/1", (y * per_mm).round() as i64),
        );
    }
    let body = fields
        .iter()
        .map(|(k, v)| {
            format!(
                "    {}: {{\"value\": {}}}",
                serde_json::Value::from(k.as_str()),
                serde_json::Value::from(v.as_str())
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    format!("{{\n{body}}}").into_bytes()
}

/// 从 libvips 的 EXIF 文本里取出原始值。
///
/// libvips 写成 `<原始值> (<libexif 可读文本>, <类型>, <n> components, <m> bytes)`；原始值是数字、
/// 有理数（`96/1`）或字符串。ASCII 类型的可读文本与原始值相同，原始值本身可能含 ` (`，所以按
/// “值 + ` (` + 同一个值”的对称结构切；其他类型的原始值不含空格加括号，取第一个 ` (` 之前。
fn exif_value(text: &str) -> &str {
    let Some(inner) = text.strip_suffix(" bytes)") else {
        return text.trim();
    };
    let mut parts = inner.rsplitn(4, ", ");
    let (_bytes, _count, kind, rest) = (parts.next(), parts.next(), parts.next(), parts.next());
    let Some(rest) = rest else {
        return text.trim();
    };
    if kind == Some("ASCII") && rest.len() >= 2 && (rest.len() - 2) % 2 == 0 {
        let half = (rest.len() - 2) / 2;
        if let (Some(a), Some(sep), Some(b)) = (
            rest.get(..half),
            rest.get(half..half + 2),
            rest.get(half + 2..),
        ) && sep == " ("
            && a == b
        {
            return a;
        }
    }
    rest.split_once(" (").map_or(rest, |(value, _)| value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{Plan, parse};

    fn step(value: &str) -> Step {
        match parse(value).unwrap() {
            Plan::Image(mut steps) => steps.remove(0),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn worker_arguments_are_a_normalized_plan() {
        assert_eq!(
            step_args(&step(
                "image/auto-orient,1/resize,m_lfit,w_256,h_256,limit_1/quality,q_90/format,webp"
            )),
            [
                "1", "image", "webp", "rel", "90", "0", "resize", "lfit", "256", "256", "0", "0",
                "0", "1", "FFFFFF"
            ]
        );
        // 参数顺序、数字写法、默认值写不写都不影响规范化结果。
        assert_eq!(
            step_args(&step(
                "image/format,webp/resize,h_0100,w_+256,m_lfit/quality,q_90"
            )),
            step_args(&step(
                "image/auto-orient,1/resize,m_lfit,w_256,h_100,limit_1/quality,q_90/format,webp"
            ))
        );
        assert_ne!(
            step_args(&step("image/resize,m_fill,w_256,h_256")),
            step_args(&step("image/resize,m_lfit,w_256,h_256"))
        );
        assert_eq!(
            step_args(&step(
                "image/crop,g_se,x_-1,w_5/rotate,90/indexcrop,y_10,i_2/blur,r_3,s_2/bright,-5/contrast,6/sharpen,100/circle,r_7/rounded-corners,r_8/interlace,1/auto-orient,0"
            )),
            [
                "0",
                "image",
                "source",
                "none",
                "0",
                "1",
                "crop",
                "5",
                "0",
                "-1",
                "0",
                "se",
                "rotate",
                "90",
                "indexcrop",
                "y",
                "10",
                "2",
                "blur",
                "3",
                "2",
                "bright",
                "-5",
                "contrast",
                "6",
                "sharpen",
                "100",
                "circle",
                "7",
                "rounded",
                "8"
            ]
        );
        assert_eq!(step_args(&step("image/info"))[1], "info");
        assert_eq!(step_args(&step("image/average-hue"))[1], "hue");
    }

    #[test]
    fn magic_bytes_decide_the_source_format() {
        assert_eq!(source_format(&[0xff, 0xd8, 0xff, 0xe0]), Some(Format::Jpg));
        assert_eq!(source_format(b"\x89PNG\r\n\x1a\n...."), Some(Format::Png));
        assert_eq!(source_format(b"RIFF\0\0\0\0WEBPVP8 "), Some(Format::Webp));
        assert_eq!(source_format(b"\0\0\0\x1cftypavif"), Some(Format::Avif));
        assert_eq!(source_format(b"\0\0\0\x18ftypheic"), Some(Format::Heic));
        for bytes in [
            &b"<svg onload='alert(1)'/>"[..],
            b"%PDF-1.7",
            b"",
            b"\0\0\0\x18ftypmp42",
        ] {
            assert_eq!(source_format(bytes), None);
        }
    }

    #[test]
    fn info_json_is_byte_compatible_with_oss() {
        // OSS 示例桶 example.jpg（无 EXIF、JFIF 96 DPI）的实测应答，共 290 字节。
        let raw = "width\t400\nheight\t300\nframes\t1\nresunit\tin\nxres\t3.7795275590551185\nyres\t3.7795275590551185\n";
        let expected = "{\n    \"FileSize\": {\"value\": \"24734\"},\n    \"Format\": {\"value\": \"jpg\"},\n    \"FrameCount\": {\"value\": \"1\"},\n    \"ImageHeight\": {\"value\": \"300\"},\n    \"ImageWidth\": {\"value\": \"400\"},\n    \"ResolutionUnit\": {\"value\": \"2\"},\n    \"XResolution\": {\"value\": \"96/1\"},\n    \"YResolution\": {\"value\": \"96/1\"}}";
        let json = info_json(raw, 24734, Format::Jpg);
        assert_eq!(String::from_utf8(json.clone()).unwrap(), expected);
        assert_eq!(json.len(), 290);
        serde_json::from_slice::<serde_json::Value>(&json).unwrap();
    }

    #[test]
    fn info_json_merges_exif_and_escapes_values() {
        let raw = "width\t424\nheight\t333\nframes\t1\n\
            exif\tifd0\tOrientation\t7 (Left-bottom, Short, 1 components, 2 bytes)\n\
            exif\tifd0\tXResolution\t96/1 (96, Rational, 1 components, 8 bytes)\n\
            exif\tifd1\tXResolution\t72/1 (72, Rational, 1 components, 8 bytes)\n\
            exif\tifd0\tImageWidth\t9999 (9999, Long, 1 components, 4 bytes)\n\
            exif\tifd0\tSoftware\tA \"b\" (c) (A \"b\" (c), ASCII, 12 components, 12 bytes)\n";
        let json: serde_json::Value =
            serde_json::from_slice(&info_json(raw, 10, Format::Jpg)).unwrap();
        assert_eq!(json["Orientation"]["value"], "7");
        // IFD0 先出现，缩略图 IFD1 的同名标签不覆盖它。
        assert_eq!(json["XResolution"]["value"], "96/1");
        // 有解码得到的分辨率时以它为准，EXIF 原始分数不照搬（OSS 实测 96000/1000 显示为 96/1）。
        let with_density = format!(
            "{raw}resunit\tin\nxres\t3.7795275590551185\nyres\t3.7795275590551185\nexif\tifd0\tYResolution\t96000/1000 (96, Rational, 1 components, 8 bytes)\n"
        );
        let json: serde_json::Value =
            serde_json::from_slice(&info_json(&with_density, 10, Format::Jpg)).unwrap();
        assert_eq!(json["YResolution"]["value"], "96/1");
        assert_eq!(json["ImageWidth"]["value"], "424");
        assert_eq!(json["Software"]["value"], "A \"b\" (c)");
        // PNG 不返回 EXIF。
        let png: serde_json::Value =
            serde_json::from_slice(&info_json(raw, 10, Format::Png)).unwrap();
        assert!(png.get("Orientation").is_none());
        assert_eq!(png["Format"]["value"], "png");
    }

    #[test]
    fn exif_text_keeps_only_the_raw_value() {
        assert_eq!(
            exif_value("2 2 0 0 (2.2.0.0, Byte, 4 components, 4 bytes)"),
            "2 2 0 0"
        );
        assert_eq!(
            exif_value("2015:02:11 15:38:27 (2015:02:11 15:38:27, ASCII, 20 components, 20 bytes)"),
            "2015:02:11 15:38:27"
        );
        assert_eq!(exif_value("plain"), "plain");
    }
}
