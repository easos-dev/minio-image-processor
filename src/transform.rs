use crate::error::{Failure, Result};
use axum::http::StatusCode;
use image::{
    DynamicImage, ImageDecoder, ImageEncoder, ImageFormat, ImageReader, imageops::FilterType,
};
use serde::Deserialize;
use std::io::Cursor;

#[derive(Clone, Copy, Debug, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Fit {
    Fill,
    #[default]
    Contain,
    Cover,
}
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    #[serde(alias = "jpeg")]
    Jpg,
    Png,
    Webp,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parameters {
    pub url: String,
    #[serde(default = "dimension")]
    pub width: u32,
    #[serde(default = "dimension")]
    pub height: u32,
    #[serde(default = "quality")]
    pub quality: u8,
    #[serde(default)]
    pub fit: Fit,
    pub format: Option<Format>,
}
fn dimension() -> u32 {
    1920
}
fn quality() -> u8 {
    90
}
impl Parameters {
    pub fn validate(&self) -> Result<()> {
        if !(1..=1920).contains(&self.width)
            || !(1..=1920).contains(&self.height)
            || !(1..=100).contains(&self.quality)
        {
            return Err(Failure::invalid("invalid_transform"));
        }
        Ok(())
    }
}
pub struct Output {
    pub bytes: Vec<u8>,
    pub content_type: &'static str,
    pub width: u32,
    pub height: u32,
}

/// 先探测真实格式和像素预算，再解码及 EXIF 旋转，最后缩放和重新编码。
/// 不信任 Content-Type，不执行 SVG，不读文件路径；重编码不保留 EXIF/GPS 等元数据。
pub fn transform(bytes: &[u8], params: &Parameters, max_pixels: u64) -> Result<Output> {
    params.validate()?;
    let invalid = || Failure(StatusCode::UNPROCESSABLE_ENTITY, "invalid_image");
    let format = image::guess_format(bytes).map_err(|_| invalid())?;
    if !matches!(
        format,
        ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP
    ) {
        return Err(Failure(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_image_format",
        ));
    }
    let animated = match format {
        ImageFormat::WebP => image::codecs::webp::WebPDecoder::new(Cursor::new(bytes))
            .map_err(|_| invalid())?
            .has_animation(),
        ImageFormat::Png => image::codecs::png::PngDecoder::new(Cursor::new(bytes))
            .map_err(|_| invalid())?
            .is_apng()
            .map_err(|_| invalid())?,
        _ => false,
    };
    if animated {
        return Err(Failure(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "animated_image_not_supported",
        ));
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(32768);
    limits.max_image_height = Some(32768);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|_| invalid())?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > max_pixels {
        return Err(Failure(StatusCode::PAYLOAD_TOO_LARGE, "source_pixel_limit"));
    }
    let orientation = decoder.orientation().map_err(|_| invalid())?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(|_| invalid())?;
    image.apply_orientation(orientation);
    let image = match params.fit {
        Fit::Fill => {
            // image 的滤波先纵向采样，浮点中间图宽度仍是原宽；同时限制中间
            // 像素数，不能只检查输入和最终输出尺寸。
            if u64::from(image.width()) * u64::from(params.height) > max_pixels {
                return Err(Failure(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "intermediate_pixel_limit",
                ));
            }
            image.resize_exact(params.width, params.height, FilterType::Lanczos3)
        }
        Fit::Contain => image.resize(
            params.width.min(image.width()),
            params.height.min(image.height()),
            FilterType::Lanczos3,
        ),
        Fit::Cover => {
            // 先裁原图再缩放；resize_to_fill 会先放大整幅长图，极端宽高比可能
            // 产生远超最终 1920×1920 的中间缓冲区。交叉乘积用 u64 防溢出。
            let (w, h) = (image.width(), image.height());
            let (cw, ch) = if u64::from(w) * u64::from(params.height)
                > u64::from(h) * u64::from(params.width)
            {
                (
                    (u64::from(h) * u64::from(params.width) / u64::from(params.height)).max(1)
                        as u32,
                    h,
                )
            } else {
                (
                    w,
                    (u64::from(w) * u64::from(params.height) / u64::from(params.width)).max(1)
                        as u32,
                )
            };
            image
                .crop_imm((w - cw) / 2, (h - ch) / 2, cw, ch)
                .resize_exact(params.width, params.height, FilterType::Lanczos3)
        }
    };
    let (width, height) = (image.width(), image.height());
    let output_format = params.format.unwrap_or(match format {
        ImageFormat::Jpeg => Format::Jpg,
        ImageFormat::WebP => Format::Webp,
        _ => Format::Png,
    });
    let mut output = Vec::new();
    let failed = |_| Failure(StatusCode::UNPROCESSABLE_ENTITY, "image_encode_failed");
    let content_type = match output_format {
        Format::Jpg => {
            // JPEG 不支持透明度；先合成白底，避免透明区域意外变黑。
            let rgba = image.to_rgba8();
            let mut rgb = image::RgbImage::new(width, height);
            for (dst, src) in rgb.pixels_mut().zip(rgba.pixels()) {
                let a = u32::from(src[3]);
                for channel in 0..3 {
                    dst[channel] =
                        ((u32::from(src[channel]) * a + 255 * (255 - a) + 127) / 255) as u8;
                }
            }
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, params.quality)
                .encode(rgb.as_raw(), width, height, image::ExtendedColorType::Rgb8)
                .map_err(failed)?;
            "image/jpeg"
        }
        Format::Png => {
            let rgba = image.to_rgba8();
            image::codecs::png::PngEncoder::new(&mut output)
                .write_image(
                    rgba.as_raw(),
                    width,
                    height,
                    image::ExtendedColorType::Rgba8,
                )
                .map_err(failed)?;
            "image/png"
        }
        Format::Webp => {
            let rgba = image.to_rgba8();
            // 使用 libwebp 有损编码，质量参数真正参与编码；PNG 则始终无损。
            output = webp::Encoder::from_rgba(rgba.as_raw(), width, height)
                .encode(f32::from(params.quality))
                .to_vec();
            "image/webp"
        }
    };
    if output.len() > 20 * 1024 * 1024 {
        return Err(Failure(StatusCode::PAYLOAD_TOO_LARGE, "output_too_large"));
    }
    Ok(Output {
        bytes: output,
        content_type,
        width,
        height,
    })
}
