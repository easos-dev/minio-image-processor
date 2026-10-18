//! 原生编码子进程的处理语义：几何尺寸、格式、质量、动图、EXIF、info/average-hue 与错误映射。
//!
//! 期望尺寸取自 OSS 公开示例桶对 400×300 原图的实测结果（docs/protocol.md 有对照记录）。
//! 需要目标 Linux 环境的 libvips 与 image-worker（见 README 的检查命令）。
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use image_processor::{
    process::{Plan, Step, parse},
    transform::{self, Product},
};
use std::io::Cursor;

fn encode(image: DynamicImage, format: ImageFormat) -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    image.write_to(&mut bytes, format).unwrap();
    bytes.into_inner()
}
/// 带渐变的不透明 PNG，避免纯色图让编码结果过于特殊。
fn png(width: u32, height: u32) -> Vec<u8> {
    encode(
        DynamicImage::ImageRgba8(RgbaImage::from_fn(width, height, |x, y| {
            Rgba([(x * 7) as u8, (y * 11) as u8, 80, 255])
        })),
        ImageFormat::Png,
    )
}
fn jpeg(width: u32, height: u32, quality: u8) -> Vec<u8> {
    let image = DynamicImage::ImageRgba8(RgbaImage::from_fn(width, height, |x, y| {
        Rgba([(x * 7) as u8, (y * 11) as u8, ((x + y) * 3) as u8, 255])
    }))
    .to_rgb8();
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, quality)
        .encode_image(&image)
        .unwrap();
    bytes
}
fn steps(value: &str) -> Vec<Step> {
    match parse(value).unwrap() {
        Plan::Image(steps) => steps,
        other => panic!("{value}: {other:?}"),
    }
}
fn run(input: &[u8], value: &str) -> Product {
    transform::run(input.to_vec(), &steps(value)).unwrap_or_else(|e| panic!("{value}: {e:?}"))
}
fn error(input: &[u8], value: &str) -> image_processor::error::OssError {
    transform::run(input.to_vec(), &steps(value)).expect_err(value)
}
/// 用通用解码器核对实际像素尺寸，不只相信子进程报告的数字。
fn decoded(product: &Product) -> (u32, u32) {
    let image = image::load_from_memory(&product.bytes).unwrap();
    (image.width(), image.height())
}
fn info(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(&run(bytes, "image/info").bytes).unwrap()
}

#[test]
fn geometry_matches_oss_measurements_on_400x300() {
    let input = png(400, 300);
    for (value, size) in [
        ("resize,m_lfit,w_150,h_80", (107, 80)),
        ("resize,m_mfit,w_150,h_80", (150, 113)),
        ("resize,m_fill,w_150,h_80", (150, 80)),
        ("resize,m_pad,w_150,h_80", (150, 80)),
        ("resize,m_fixed,w_150,h_80", (150, 80)),
        // limit_1（默认）：需要放大时保持原尺寸；pad 不受 limit 约束。
        ("resize,m_fill,w_500,h_100", (400, 300)),
        ("resize,m_lfit,w_500,h_100", (133, 100)),
        ("resize,m_mfit,w_500,h_100", (400, 300)),
        ("resize,m_fixed,w_500,h_100", (400, 300)),
        ("resize,m_pad,w_500,h_100", (500, 100)),
        ("resize,m_pad,w_500,h_500", (500, 500)),
        ("resize,w_500", (400, 300)),
        ("resize,w_500,limit_2", (400, 300)),
        ("resize,w_500,limit_0", (500, 375)),
        // 单边：fill/pad 按正方形框，lfit/mfit/fixed 按该边等比。
        ("resize,m_fill,w_100", (100, 100)),
        ("resize,m_pad,h_100", (100, 100)),
        ("resize,m_fixed,w_100", (100, 75)),
        ("resize,w_100,h_100,l_50", (100, 75)),
        ("resize,l_200", (200, 150)),
        ("resize,s_200", (267, 200)),
        ("resize,s_200,limit_0", (267, 200)),
        ("resize,l_200,s_100", (133, 100)),
        ("resize,m_fill,l_200,s_100", (200, 100)),
        ("resize,m_fixed,l_200,s_100", (200, 100)),
        ("resize,w_0,l_100", (100, 75)),
        ("resize,p_50", (200, 150)),
        ("resize,p_120", (480, 360)),
        ("resize,p_50,w_100", (100, 75)),
        ("resize,p_50,l_100", (100, 75)),
        ("resize,m_lfit,w_150,h_80,p_50", (107, 80)),
        ("resize,w_200/resize,w_100", (100, 75)),
        ("resize,w_100/resize,w_200", (100, 75)),
        ("crop", (400, 300)),
        ("crop,x_399", (1, 300)),
        ("crop,w_100,h_100", (100, 100)),
        ("crop,x_350,y_250,w_100,h_100", (50, 50)),
        ("crop,g_se,w_100,h_100", (100, 100)),
        ("crop,g_se,x_10,y_10,w_100,h_100", (90, 90)),
        ("crop,g_center,x_10,y_10,w_100,h_100", (100, 100)),
        ("crop,g_nw,x_10,y_10,w_100,h_100", (100, 100)),
        ("crop,g_se,x_-10,y_-10,w_100,h_100", (100, 100)),
        ("crop,w_0,h_0", (400, 300)),
        ("crop,w_500,h_500", (400, 300)),
        ("crop,x_-1", (400, 300)),
        ("crop,foo_1,w_100", (100, 300)),
        ("rotate,90", (300, 400)),
        ("rotate,45", (494, 494)),
        ("rotate,200", (478, 418)),
        ("rotate,0", (400, 300)),
        ("rotate,360", (400, 300)),
        ("circle,r_100", (201, 201)),
        ("circle,r_1000", (299, 299)),
        ("rounded-corners,r_30", (400, 300)),
        ("rounded-corners,r_1000", (400, 300)),
        ("indexcrop,x_100,i_0", (100, 300)),
        ("indexcrop,x_100,i_3", (100, 300)),
        ("indexcrop,x_100,i_4", (400, 300)),
        ("indexcrop,x_100,i_10", (400, 300)),
        ("indexcrop,y_100,i_1", (400, 100)),
        ("indexcrop,x_100,y_100,i_1", (400, 100)),
        ("indexcrop,x_500,i_0", (400, 300)),
        ("blur,r_3,s_2", (400, 300)),
        ("bright,50", (400, 300)),
        ("contrast,-50", (400, 300)),
        ("sharpen,100", (400, 300)),
    ] {
        let product = run(&input, &format!("image/{value}"));
        assert_eq!((product.width, product.height), size, "{value}");
        assert_eq!(decoded(&product), size, "{value}");
        assert_eq!(product.content_type, "image/png", "{value}");
    }
}

#[test]
fn easos_file_previews_shrink_large_photos_to_webp() {
    // 迁入的真实相机照片包含 8192×5464；用 shrink-on-load 缩略图，不必完整解码。
    let photo = encode(DynamicImage::new_rgb8(8192, 5464), ImageFormat::Jpeg);
    for (mode, size) in [("lfit", (256, 171)), ("fill", (256, 256))] {
        let product = run(
            &photo,
            &format!(
                "image/auto-orient,1/resize,m_{mode},w_256,h_256,limit_1/quality,q_90/format,webp"
            ),
        );
        assert_eq!(product.content_type, "image/webp");
        assert_eq!(decoded(&product), size, "{mode}");
    }
    // 原图比档位小：两种都不放大，b 的结果不是正方形。
    let small = png(120, 60);
    for (mode, size) in [("lfit", (120, 60)), ("fill", (120, 60))] {
        let product = run(
            &small,
            &format!(
                "image/auto-orient,1/resize,m_{mode},w_256,h_256,limit_1/quality,q_90/format,webp"
            ),
        );
        assert_eq!(decoded(&product), size, "{mode}");
    }
}

#[test]
fn rotation_is_clockwise_and_fills_new_corners_white() {
    // 左半红、右半蓝；顺时针旋转后红在上、蓝在下。
    let image = DynamicImage::ImageRgba8(RgbaImage::from_fn(30, 10, |x, _| {
        if x < 15 {
            Rgba([255, 0, 0, 255])
        } else {
            Rgba([0, 0, 255, 255])
        }
    }));
    let input = encode(image, ImageFormat::Png);
    for angle in ["90", "89"] {
        let out = image::load_from_memory(&run(&input, &format!("image/rotate,{angle}")).bytes)
            .unwrap()
            .to_rgb8();
        let (w, h) = out.dimensions();
        let top = out.get_pixel(w / 2, h / 4).0;
        let bottom = out.get_pixel(w / 2, h * 3 / 4).0;
        assert!(top[0] > 200 && top[2] < 60, "{angle}: {top:?}");
        assert!(bottom[2] > 200 && bottom[0] < 60, "{angle}: {bottom:?}");
    }
    // 不透明源（无透明通道）：新出现的角填白色；带透明通道的源：角为透明。
    let opaque = encode(DynamicImage::new_rgb8(400, 300), ImageFormat::Png);
    let corner = image::load_from_memory(&run(&opaque, "image/rotate,45").bytes)
        .unwrap()
        .to_rgba8();
    assert_eq!(corner.dimensions(), (494, 494));
    assert_eq!(corner.get_pixel(0, 0).0, [255, 255, 255, 255]);
    let corner = image::load_from_memory(&run(&png(400, 300), "image/rotate,45").bytes)
        .unwrap()
        .to_rgba8();
    assert_eq!(corner.get_pixel(0, 0)[3], 0);
}

#[test]
fn formats_content_types_and_alpha_follow_oss() {
    let input = png(120, 60);
    for (format, mime) in [
        ("jpg", "image/jpeg"),
        ("jpeg", "image/jpeg"),
        ("png", "image/png"),
        ("webp", "image/webp"),
        ("bmp", "image/x-ms-bmp"),
        ("tiff", "image/tiff"),
        ("heic", "image/heic"),
        ("avif", "image/avif"),
        // gif 只对 GIF 源生效，其他源保持原格式。
        ("gif", "image/png"),
    ] {
        let product = run(&input, &format!("image/resize,w_40/format,{format}"));
        assert_eq!(product.content_type, mime, "{format}");
        // 每种结果都能被再次读回，尺寸一致。
        let back = run(&product.bytes, "image/format,png");
        assert_eq!((back.width, back.height), (40, 20), "{format}");
    }
    // 管道：前一段的编码结果是后一段的输入。
    let piped = run(&input, "image/resize,w_40|image/format,png");
    assert_eq!((piped.width, piped.content_type), (40, "image/png"));
    // 透明 PNG 转 JPEG 白底；circle 外部在 PNG 里透明、在 JPEG 里为白色。
    let transparent = encode(DynamicImage::new_rgba8(4, 4), ImageFormat::Png);
    let flat = image::load_from_memory(&run(&transparent, "image/format,jpg").bytes)
        .unwrap()
        .to_rgb8();
    assert!(flat.get_pixel(0, 0).0.iter().all(|v| *v > 245));
    let circle =
        image::load_from_memory(&run(&png(100, 100), "image/circle,r_40/format,png").bytes)
            .unwrap()
            .to_rgba8();
    assert_eq!(circle.dimensions(), (81, 81));
    assert_eq!(circle.get_pixel(0, 0)[3], 0);
    assert_eq!(circle.get_pixel(40, 40)[3], 255);
    let white = image::load_from_memory(&run(&png(100, 100), "image/circle,r_40/format,jpg").bytes)
        .unwrap()
        .to_rgb8();
    assert!(white.get_pixel(0, 0).0.iter().all(|v| *v > 245));
    let rounded =
        image::load_from_memory(&run(&png(100, 60), "image/rounded-corners,r_20/format,png").bytes)
            .unwrap()
            .to_rgba8();
    assert_eq!(rounded.get_pixel(0, 0)[3], 0);
    assert_eq!(rounded.get_pixel(50, 30)[3], 255);
}

#[test]
fn quality_follows_oss_relative_and_absolute_rules() {
    let source = jpeg(200, 150, 75);
    let default = run(&source, "image/format,jpg");
    // JPEG：Q 取 min(源质量, Q)，源质量约 75 时 Q_90 与缺省相同；q 相对源质量缩小。
    assert_eq!(run(&source, "image/quality,Q_90").bytes, default.bytes);
    assert_ne!(run(&source, "image/quality,Q_50").bytes, default.bytes);
    assert!(
        run(&source, "image/quality,q_50").bytes.len()
            < run(&source, "image/quality,q_90").bytes.len()
    );
    // WebP：缺省 min(源质量, 80)，Q 是绝对质量。
    let webp = run(&source, "image/format,webp");
    assert_eq!(
        run(&source, "image/format,webp/quality,Q_75").bytes,
        webp.bytes
    );
    assert_ne!(
        run(&source, "image/format,webp/quality,Q_90").bytes,
        webp.bytes
    );
    assert_ne!(
        run(&source, "image/format,webp/quality,q_90").bytes,
        webp.bytes
    );
    // PNG 无损，quality 不起作用。
    let png_source = png(64, 64);
    assert_eq!(
        run(&png_source, "image/quality,q_10").bytes,
        run(&png_source, "image/format,png").bytes
    );
    // 渐进 JPEG 与普通 JPEG 字节不同。
    assert_ne!(run(&source, "image/interlace,1").bytes, default.bytes);
}

#[test]
fn exif_orientation_is_applied_by_default_and_stripped() {
    let base = encode(DynamicImage::new_rgb8(3, 2), ImageFormat::Jpeg);
    // APP1 内嵌最小 little-endian TIFF，Orientation=6（顺时针 90 度）。
    let exif = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
    let mut source = base[..2].to_vec();
    source.extend([0xff, 0xe1]);
    source.extend(((exif.len() + 2) as u16).to_be_bytes());
    source.extend(exif);
    source.extend(&base[2..]);
    for (value, size) in [
        ("image/resize,w_3,h_3", (2, 3)),
        ("image/auto-orient,1/resize,w_3,h_3", (2, 3)),
        ("image/resize,w_3,h_3/auto-orient,0", (3, 2)),
        ("image/format,png", (2, 3)),
        ("image/auto-orient,0/format,png", (3, 2)),
    ] {
        let product = run(&source, value);
        assert_eq!(decoded(&product), size, "{value}");
        assert!(
            !product.bytes.windows(6).any(|w| w == b"Exif\0\0"),
            "{value}"
        );
    }
    // info 报原始尺寸与 EXIF 原始值。
    let json = info(&source);
    assert_eq!(json["ImageWidth"]["value"], "3");
    assert_eq!(json["ImageHeight"]["value"], "2");
    assert_eq!(json["Orientation"]["value"], "6");
    assert_eq!(json["Format"]["value"], "jpg");
}

#[test]
fn animation_survives_resize_and_crop_only() {
    let mut gif = Vec::new();
    {
        let mut encoder = image::codecs::gif::GifEncoder::new(&mut gif);
        encoder
            .set_repeat(image::codecs::gif::Repeat::Infinite)
            .unwrap();
        for red in [128, 255] {
            encoder
                .encode_frame(image::Frame::from_parts(
                    RgbaImage::from_pixel(40, 20, Rgba([red, 0, 0, 255])),
                    0,
                    0,
                    image::Delay::from_numer_denom_ms(100, 1),
                ))
                .unwrap();
        }
    }
    assert_eq!(info(&gif)["FrameCount"]["value"], "2");
    for value in [
        "image/resize,w_20",
        "image/resize,m_fill,w_10,h_10/crop,w_5",
        "image/resize,w_20/format,webp",
    ] {
        let product = run(&gif, value);
        assert_eq!(info(&product.bytes)["FrameCount"]["value"], "2", "{value}");
    }
    let resized = run(&gif, "image/resize,w_20");
    assert_eq!(
        (resized.width, resized.height, resized.content_type),
        (20, 10, "image/gif")
    );
    // 其他操作或非动图格式只留第一帧。
    for value in [
        "image/rotate,90",
        "image/blur,r_2,s_2",
        "image/resize,w_20/format,png",
    ] {
        let product = run(&gif, value);
        assert_eq!(info(&product.bytes)["FrameCount"]["value"], "1", "{value}");
    }
}

#[test]
fn info_and_average_hue_describe_the_source() {
    let json = info(&png(120, 60));
    assert_eq!(json["Format"]["value"], "png");
    assert_eq!(json["ImageWidth"]["value"], "120");
    assert_eq!(json["ImageHeight"]["value"], "60");
    assert_eq!(json["FrameCount"]["value"], "1");
    assert_eq!(json["FileSize"]["value"], png(120, 60).len().to_string());
    // 处理过的输出不再带源图元数据。
    let solid = encode(
        DynamicImage::ImageRgba8(RgbaImage::from_pixel(16, 16, Rgba([0x5c, 0x78, 0x3b, 255]))),
        ImageFormat::Png,
    );
    for value in [
        "image/average-hue",
        "image/resize,w_4/average-hue",
        "image/crop,w_2/average-hue/format,png",
    ] {
        let product = run(&solid, value);
        assert_eq!(product.content_type, "application/json");
        assert_eq!(
            std::str::from_utf8(&product.bytes).unwrap(),
            "{\"RGB\": \"0x5c783b\"}",
            "{value}"
        );
    }
}

/// 只写 PNG 签名 + IHDR + 一个极小的 IDAT + IEND，声明巨大尺寸但几乎不含像素：限额检查必须在解码之前生效。
fn png_header(width: u32, height: u32) -> Vec<u8> {
    fn crc(data: &[u8]) -> u32 {
        let mut c = 0xffff_ffffu32;
        for b in data {
            c ^= *b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xedb8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
        !c
    }
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    for (kind, data) in [
        (
            &b"IHDR"[..],
            [width.to_be_bytes(), height.to_be_bytes()]
                .concat()
                .into_iter()
                .chain([8, 6, 0, 0, 0])
                .collect::<Vec<u8>>(),
        ),
        // zlib 存储块：只含一个字节 0，Adler-32 为 0x00010001。
        (
            &b"IDAT"[..],
            vec![
                0x78, 0x01, 0x01, 0x01, 0x00, 0xfe, 0xff, 0x00, 0x00, 0x01, 0x00, 0x01,
            ],
        ),
        (&b"IEND"[..], vec![]),
    ] {
        out.extend((data.len() as u32).to_be_bytes());
        let mut chunk = kind.to_vec();
        chunk.extend(&data);
        out.extend(&chunk);
        out.extend(crc(&chunk).to_be_bytes());
    }
    out
}

#[test]
fn limits_and_damaged_images_map_to_oss_errors() {
    let check = |input: &[u8], value: &str, code: &str, ec: Option<&str>| {
        let e = error(input, value);
        assert_eq!(
            (e.status.as_u16(), e.code, e.ec),
            (400, code, ec),
            "{value}: {e:?}"
        );
    };
    check(
        &png_header(20000, 20000),
        "image/resize,w_100",
        "ImageTooLarge",
        Some("0040-00000012"),
    );
    check(
        &png_header(30001, 1),
        "image/resize,w_100",
        "ImageTooLarge",
        Some("0040-00000009"),
    );
    check(
        &png_header(20000, 20000),
        "image/info",
        "ImageTooLarge",
        Some("0040-00000012"),
    );
    let mut broken = png(20, 20);
    broken.truncate(40);
    check(
        &broken,
        "image/resize,w_10",
        "BadRequest",
        Some("0040-00000002"),
    );
    let mut webp = encode(DynamicImage::new_rgb8(8, 8), ImageFormat::WebP);
    webp.truncate(24);
    check(
        &webp,
        "image/format,png",
        "BadWebPImage",
        Some("0040-00000010"),
    );
    check(
        &png(4097, 1),
        "image/rotate,90",
        "ImageTooLarge",
        Some("0040-00000006"),
    );
    check(
        &png(100, 100),
        "image/resize,m_fixed,w_5000,h_5000,limit_0",
        "BadRequest",
        Some("0040-00000007"),
    );
    check(
        &png(16384, 1),
        "image/format,webp",
        "BadRequest",
        Some("0040-00000011"),
    );
    check(
        &png(4097, 1),
        "image/format,heic",
        "BadRequest",
        Some("0040-00000014"),
    );
    check(
        &png(400, 300),
        "image/crop,x_400",
        "CutEdgeExceedRange",
        None,
    );
    check(
        &png(400, 300),
        "image/crop,y_300",
        "CutEdgeExceedRange",
        None,
    );
    check(
        b"<svg onload='alert(1)'/>",
        "image/resize,w_10",
        "BadRequest",
        Some("0040-00000005"),
    );
}
