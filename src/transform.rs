use crate::error::{Failure, Result};
use axum::http::StatusCode;
use serde::Deserialize;
use std::{
    io::Write,
    process::{Command, Stdio},
};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parameters {
    pub url: String,
    #[serde(default = "dimension")]
    pub w: u32,
    #[serde(default = "dimension")]
    pub h: u32,
    #[serde(default = "quality")]
    pub q: f64,
    #[serde(default = "resize")]
    pub r: u8,
    pub f: Option<String>,
    pub d: Option<String>,
}
fn dimension() -> u32 {
    1920
}
fn quality() -> f64 {
    0.9
}
fn resize() -> u8 {
    1
}
impl Parameters {
    pub fn validate(&self) -> Result<()> {
        if !(1..=1920).contains(&self.w)
            || !(1..=1920).contains(&self.h)
            || !self.q.is_finite()
            || !(0.01..=1.0).contains(&self.q)
            || self.r > 2
            || self.d.as_deref().is_some_and(|d| d != "0")
            || self.f.as_deref().is_some_and(|f| !FORMATS.contains(&f))
        {
            return Err(Failure::invalid("invalid_transform"));
        }
        Ok(())
    }
    /// 规范化整数质量，缓存键不受浮点文本写法或查询顺序影响。
    pub fn quality(&self) -> u8 {
        (self.q * 100.0).round() as u8
    }
    pub fn cache_variant(&self) -> String {
        format!(
            "v2:{}:{}:{}:{}:{}",
            self.w,
            self.h,
            self.quality(),
            self.r,
            self.f.as_deref().unwrap_or("source")
        )
    }
}
pub const FORMATS: &[&str] = &["jpeg", "png", "webp", "bmp", "gif", "tiff", "heic", "avif"];
#[derive(Clone)]
pub struct Output {
    pub bytes: Vec<u8>,
    pub content_type: &'static str,
    pub width: u32,
    pub height: u32,
}
/// 仅允许明确的栅格魔数，SVG/PDF/URL/脚本不能进入通用图像加载器。
fn source_format(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[255, 216, 255]) {
        Some("jpeg")
    } else if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("gif")
    } else if b.starts_with(b"BM") {
        Some("bmp")
    } else if b.starts_with(b"II*\0")
        || b.starts_with(b"MM\0*")
        || b.starts_with(b"II+\0")
        || b.starts_with(b"MM\0+")
    {
        Some("tiff")
    } else if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        Some("webp")
    } else if b.get(4..8) == Some(b"ftyp") {
        match b.get(8..12)? {
            b"avif" | b"avis" => Some("avif"),
            b"heic" | b"heix" | b"hevc" | b"hevx" | b"mif1" | b"msf1" => Some("heic"),
            _ => None,
        }
    } else {
        None
    }
}
/// 子进程只接收父进程生成的临时路径和已校验标量，不使用 shell。
/// URL/Host/签名留在网络层；stderr 丢弃避免 codec 打印不受信的元数据。
/// 工作线程持有并发名额直至子进程退出，客户端取消不能突破资源上限。
pub fn transform(bytes: &[u8], params: &Parameters, max_pixels: u64) -> Result<Output> {
    params.validate()?;
    let source = source_format(bytes).ok_or(Failure(
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "unsupported_image_format",
    ))?;
    let failed = || Failure(StatusCode::UNPROCESSABLE_ENTITY, "invalid_image");
    let directory = tempfile::tempdir().map_err(|_| Failure::upstream("worker_unavailable"))?;
    let input = directory.path().join(format!("input.{source}"));
    let output = directory.path().join("output");
    std::fs::File::create(&input)
        .and_then(|mut f| f.write_all(bytes))
        .map_err(|_| failed())?;
    let worker = std::env::var_os("IMAGE_PROCESSOR_WORKER")
        .unwrap_or_else(|| "/usr/local/bin/image-worker".into());
    let result = Command::new(worker)
        .arg(&input)
        .arg(&output)
        .args([
            params.w.to_string(),
            params.h.to_string(),
            params.quality().to_string(),
            params.r.to_string(),
            params.f.clone().unwrap_or_else(|| "source".into()),
            source.into(),
            max_pixels.to_string(),
        ])
        .env("VIPS_CONCURRENCY", "1")
        .env("MAGICK_THREAD_LIMIT", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|_| Failure::upstream("worker_unavailable"))?;
    if !result.status.success() {
        return Err(match result.status.code() {
            Some(5) => Failure(StatusCode::PAYLOAD_TOO_LARGE, "source_pixel_limit"),
            Some(6) => Failure(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_conversion"),
            None => Failure(StatusCode::UNPROCESSABLE_ENTITY, "processing_limit"),
            _ => failed(),
        });
    }
    let info = String::from_utf8(result.stdout).map_err(|_| failed())?;
    let parts: Vec<_> = info.split_whitespace().collect();
    if parts.len() != 3 {
        return Err(failed());
    }
    let mime = match parts[2] {
        "image/jpeg" => "image/jpeg",
        "image/png" => "image/png",
        "image/webp" => "image/webp",
        "image/gif" => "image/gif",
        "image/tiff" => "image/tiff",
        "image/bmp" => "image/bmp",
        "image/heic" => "image/heic",
        "image/avif" => "image/avif",
        _ => return Err(failed()),
    };
    let bytes = std::fs::read(output).map_err(|_| failed())?;
    if bytes.is_empty() || bytes.len() > 20 * 1024 * 1024 {
        return Err(failed());
    }
    Ok(Output {
        bytes,
        content_type: mime,
        width: parts[0].parse().map_err(|_| failed())?,
        height: parts[1].parse().map_err(|_| failed())?,
    })
}
