//! `x-oss-process` 处理串解析。
//!
//! 语法、默认值、取值范围、错误码（含 EC 与 Message 原文）对齐阿里云 OSS：以官方文档为准，文档没写或与
//! 线上行为不一致的细节，以 OSS 公开示例桶的实测应答为准（对照记录见 docs/protocol.md）。解析只看处理串，
//! 不依赖图片；与图片尺寸有关的判断（裁剪越界、旋转上限、放大限制等）在编码子进程里做。
//!
//! 解析结果是规范化的 [`Plan`]：同一语义的不同写法（参数顺序、重复参数、`+100`/`0100` 之类数字写法）
//! 得到相同结构，缓存键与编码子进程参数都由它导出。

use crate::error::{OssError, Result};

/// 管道 `|` 分隔的最多段数。OSS 实测 6 段可用、7 段返回 `TooManyPipes`。
pub const MAX_STEPS: usize = 6;
/// resize 的 w/h/l/s 上限（OSS 文档 [1,16384]，0 表示未指定）。
pub const MAX_RESIZE_SIDE: i64 = 16_384;
/// crop 的 w/h 上限，超过时 OSS 返回 `CutEdgeExceedRange`。
pub const MAX_CROP_SIDE: i64 = 16_384;

/// 整条处理串的去向。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// 不加工、原样返回原对象：`image` 本身、空值或 OSS 不认识的前缀（OSS 实测同样返回原图）。
    Original,
    /// `style/<名称>`：网关没有样式配置，按 OSS 样式不存在处理（404 `NoSuchStyle`）。
    NoSuchStyle,
    /// 一段或多段 `image/` 处理，按管道顺序执行，前一段的编码结果是后一段的输入。
    Image(Vec<Step>),
}

/// 一段 `image/...` 处理。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    /// 是否按 EXIF Orientation 转正（auto-orient）。OSS 默认 1，且与它在串里的位置无关：
    /// 解码时就决定，作用于本段所有操作之前。
    pub auto_orient: bool,
    /// 像素操作，按串里出现的先后执行。
    pub ops: Vec<Op>,
    /// 输出格式；`None` 保持本段输入的格式。多次出现取最后一次。
    pub format: Option<Format>,
    /// 输出质量；`None` 按 OSS 默认（见 docs/protocol.md 的质量规则）。多次出现取最后一次。
    pub quality: Option<Quality>,
    /// JPEG 渐进显示（interlace）。多次出现取最后一次；非 JPEG 输出忽略。
    pub interlace: bool,
    /// 本段产出什么。
    pub output: Output,
}

/// 一段处理的产物类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Output {
    /// 编码后的图片。
    Image,
    /// `info`：输入图片的基本信息与 EXIF（JSON），忽略本段其他操作。
    Info,
    /// `average-hue`：输入图片的平均色（JSON），忽略它前面的操作。
    AverageHue,
}

/// OSS `format` 支持的格式，同时也是网关能识别的源图格式（变体即格式名）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Format {
    /// JPEG（OSS 写作 jpg，jpeg 为同义写法）。
    Jpg,
    /// PNG，无损，质量参数不起作用。
    Png,
    /// WebP，动图保持多帧。
    Webp,
    /// BMP。
    Bmp,
    /// GIF，只有 GIF 源才会输出 GIF。
    Gif,
    /// TIFF（deflate 压缩）。
    Tiff,
    /// HEIF/HEIC。
    Heic,
    /// AVIF。
    Avif,
}

impl Format {
    /// OSS 使用的格式名（`format,<名>`、`info` 的 Format 字段），也是传给编码子进程的名字。
    pub fn name(self) -> &'static str {
        match self {
            Format::Jpg => "jpg",
            Format::Png => "png",
            Format::Webp => "webp",
            Format::Bmp => "bmp",
            Format::Gif => "gif",
            Format::Tiff => "tiff",
            Format::Heic => "heic",
            Format::Avif => "avif",
        }
    }

    /// 响应 Content-Type，取 OSS 实测值（BMP 为 `image/x-ms-bmp`）。
    pub fn mime(self) -> &'static str {
        match self {
            Format::Jpg => "image/jpeg",
            Format::Png => "image/png",
            Format::Webp => "image/webp",
            Format::Bmp => "image/x-ms-bmp",
            Format::Gif => "image/gif",
            Format::Tiff => "image/tiff",
            Format::Heic => "image/heic",
            Format::Avif => "image/avif",
        }
    }

    /// 解析 `format` 的取值。大小写敏感：OSS 实测 `JPG` 无效、`jpeg` 与 `jpg` 等价。
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "jpg" | "jpeg" => Format::Jpg,
            "png" => Format::Png,
            "webp" => Format::Webp,
            "bmp" => Format::Bmp,
            "gif" => Format::Gif,
            "tiff" => Format::Tiff,
            "heic" => Format::Heic,
            "avif" => Format::Avif,
            _ => return None,
        })
    }
}

/// 输出质量。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quality {
    /// true：`Q`（绝对质量）；false：`q`（相对源图质量）。同一 quality 里只要出现过 `Q` 就按绝对质量
    /// （OSS 实测 `Q_50,q_90` 与 `q_90,Q_50` 都按绝对质量处理）。
    pub absolute: bool,
    /// 取值 1–100，同一 quality 里多次给出时取最后一个数值。
    pub value: u8,
}

/// 按顺序执行的像素操作。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// 缩放。
    Resize(Resize),
    /// 自定义裁剪。
    Crop(Crop),
    /// 顺时针旋转角度，0–360。
    Rotate(u16),
    /// 内切圆半径，1–4096。
    Circle(u16),
    /// 圆角半径，1–4096。
    RoundedCorners(u16),
    /// 索引切割。
    IndexCrop {
        /// true 按 y 轴（纵向）切；false 按 x 轴切。x、y 都给时 y 生效。
        vertical: bool,
        /// 每块的边长（像素），≥1。
        size: u32,
        /// 取第几块，从 0 起。
        index: u32,
    },
    /// 高斯模糊：半径 r 与标准差 s，均为 1–50。
    Blur { radius: u8, sigma: u8 },
    /// 亮度 -100–100。
    Bright(i8),
    /// 对比度 -100–100。
    Contrast(i8),
    /// 锐化 50–399。
    Sharpen(u16),
}

/// resize 参数。所有边长为 0 表示未指定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resize {
    /// 缩放模式，默认 lfit。
    pub mode: Mode,
    /// 目标宽，0–16384。
    pub w: u32,
    /// 目标高，0–16384。
    pub h: u32,
    /// 目标长边，0–16384。只在没有 w/h 时生效。
    pub l: u32,
    /// 目标短边，0–16384。只在没有 w/h 时生效。
    pub s: u32,
    /// 百分比 1–1000，0 表示未指定。只在没有 w/h/l/s 时生效，不受 limit 约束。
    pub p: u32,
    /// limit：目标比原图大时是否返回原尺寸。默认 true；OSS 实测除 0 以外的非负整数都视为 1。
    pub limit: bool,
    /// m_pad 的填充色 RGB，默认白色。
    pub color: [u8; 3],
}

/// resize 的 m 参数。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// 等比缩放到放得进目标框的最大尺寸（默认）。
    Lfit,
    /// 等比缩放到盖满目标框的最小尺寸，不裁剪。
    Mfit,
    /// 按 mfit 缩放后居中裁成目标框。
    Fill,
    /// 按 lfit 缩放后居中放在目标框的底色上。
    Pad,
    /// 强制拉伸到目标宽高。
    Fixed,
}

impl Mode {
    /// 传给编码子进程的名字，与 OSS 取值相同。
    pub fn name(self) -> &'static str {
        match self {
            Mode::Lfit => "lfit",
            Mode::Mfit => "mfit",
            Mode::Fill => "fill",
            Mode::Pad => "pad",
            Mode::Fixed => "fixed",
        }
    }
}

/// crop 参数。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crop {
    /// 裁剪宽，0 表示到图片边界。
    pub w: u32,
    /// 裁剪高，0 表示到图片边界。
    pub h: u32,
    /// 相对锚点的横向偏移，可为负（OSS 实测负值被接受，越过左边界时按 0）。
    pub x: i64,
    /// 相对锚点的纵向偏移，可为负。
    pub y: i64,
    /// 九宫格锚点，默认 nw。
    pub gravity: Gravity,
}

/// crop 的 g 参数（九宫格），变体依次为左上、上中、右上、左中、中心、右中、左下、下中、右下。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gravity {
    /// 左上（默认）。
    Nw,
    /// 上中。
    North,
    /// 右上。
    Ne,
    /// 左中。
    West,
    /// 中心。
    Center,
    /// 右中。
    East,
    /// 左下。
    Sw,
    /// 下中。
    South,
    /// 右下。
    Se,
}

impl Gravity {
    /// 传给编码子进程的名字，与 OSS 取值相同。
    pub fn name(self) -> &'static str {
        match self {
            Gravity::Nw => "nw",
            Gravity::North => "north",
            Gravity::Ne => "ne",
            Gravity::West => "west",
            Gravity::Center => "center",
            Gravity::East => "east",
            Gravity::Sw => "sw",
            Gravity::South => "south",
            Gravity::Se => "se",
        }
    }
}

/// 解析整条 `x-oss-process` 值（已做过百分号解码）。
pub fn parse(value: &str) -> Result<Plan> {
    let segments: Vec<&str> = value.split('|').collect();
    if segments.len() > MAX_STEPS {
        return Err(OssError::bad(
            "TooManyPipes",
            "Maximal number of pipes supported is 6",
            Some("0040-00000211"),
        ));
    }
    let first = segments[0];
    if first.starts_with("style/") {
        return Ok(Plan::NoSuchStyle);
    }
    if first != "image" && !first.starts_with("image/") {
        return Ok(Plan::Original);
    }
    let mut steps = Vec::new();
    for segment in segments {
        // OSS 实测：后续段不是 image/ 时忽略（如 GET 上的 sys/saveas）；单独的 `image` 不做任何事。
        let Some(actions) = segment.strip_prefix("image/") else {
            continue;
        };
        let step = parse_step(actions)?;
        let terminal = step.output != Output::Image;
        steps.push(step);
        // info / average-hue 产出 JSON，后面的段没有图片可处理。
        if terminal {
            break;
        }
    }
    Ok(if steps.is_empty() {
        Plan::Original
    } else {
        Plan::Image(steps)
    })
}

/// 解析一段 `image/` 之后的动作列表。
fn parse_step(text: &str) -> Result<Step> {
    let mut step = Step {
        auto_orient: true,
        ops: Vec::new(),
        format: None,
        quality: None,
        interlace: false,
        output: Output::Image,
    };
    // average-hue 出现时记下当时已有的像素操作数；之后再出现像素操作即为 OSS 的 BadRequest。
    let mut hue_at: Option<usize> = None;
    let mut any = false;
    // 空动作（`//`、结尾 `/`）OSS 实测直接忽略。
    for action in text.split('/').filter(|a| !a.is_empty()) {
        any = true;
        let mut tokens = action.split(',');
        let name = tokens.next().unwrap_or_default();
        let params: Vec<&str> = tokens.collect();
        match name {
            // OSS 实测：解析到 info 立即返回源图信息，后面的动作不再解析。
            "info" => {
                step.output = Output::Info;
                return Ok(step);
            }
            "average-hue" => hue_at = Some(step.ops.len()),
            "resize" => step.ops.push(Op::Resize(resize(&params)?)),
            "crop" => step.ops.push(Op::Crop(crop(&params)?)),
            "rotate" => step.ops.push(Op::Rotate(rotate(&params)?)),
            "circle" => step.ops.push(Op::Circle(radius(&params, "0040-00000233")?)),
            "rounded-corners" => step
                .ops
                .push(Op::RoundedCorners(radius(&params, "0040-00000237")?)),
            "indexcrop" => step.ops.push(indexcrop(&params)?),
            "blur" => step.ops.push(blur(&params)?),
            "bright" => step.ops.push(Op::Bright(level(&params, "bright")?)),
            "contrast" => step.ops.push(Op::Contrast(level(&params, "contrast")?)),
            "sharpen" => step.ops.push(Op::Sharpen(sharpen(&params)?)),
            "quality" => step.quality = Some(quality(&params)?),
            "format" => step.format = Some(format(&params)?),
            "interlace" => step.interlace = flag(&params, "interlace", INTERLACE_EC)?,
            "auto-orient" => step.auto_orient = flag(&params, "auto-orient", ORIENT_EC)?,
            // watermark 需要字体与同桶水印对象，网关不提供；与其他不认识的动作一样按 OSS 报动作无效。
            _ => {
                return Err(OssError::invalid_value(name, "process", "0040-00000213"));
            }
        }
    }
    if !any {
        return Err(OssError::bad(
            "MissingArgument",
            "Missing Some Required Arguments.",
            Some("0040-00000212"),
        ));
    }
    if let Some(at) = hue_at {
        if step.ops.len() > at {
            return Err(OssError::bad("BadRequest", "Request is invalid.", None));
        }
        step.output = Output::AverageHue;
    }
    Ok(step)
}

/// OSS 的整数写法：允许前后空白、`+`/`-` 号和前导零；小数、科学计数法、空串都不是整数。
fn int(text: &str) -> Option<i64> {
    let text = text.trim();
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// `key_value` 形式的参数；没有下划线的片段返回 None（resize/crop 等对它们视而不见）。
fn pair(token: &str) -> Option<(&str, &str)> {
    token.split_once('_')
}

fn resize(params: &[&str]) -> Result<Resize> {
    let mut out = Resize {
        mode: Mode::Lfit,
        w: 0,
        h: 0,
        l: 0,
        s: 0,
        p: 0,
        limit: true,
        color: [255, 255, 255],
    };
    // OSS 实测：一个可识别的参数都没有（包括 `W_100` 这类大小写不对的键）时报 MissingArgument；
    // 有可识别参数但没有任何非零边长/百分比时报 "Width and Height can not both be 0."。
    let mut recognized = false;
    // 0–16384 的边长；0 表示未指定。数字写错与越界都是同一个 EC。
    let side = |value: &str, name: &str, ec: &'static str| -> Result<u32> {
        match int(value) {
            Some(n) if (0..=MAX_RESIZE_SIDE).contains(&n) => Ok(n as u32),
            _ => Err(OssError::invalid_value(value, name, ec)),
        }
    };
    for (key, value) in params.iter().filter_map(|t| pair(t)) {
        match key {
            "m" => {
                out.mode = match value {
                    "lfit" => Mode::Lfit,
                    "mfit" => Mode::Mfit,
                    "fill" => Mode::Fill,
                    "pad" => Mode::Pad,
                    "fixed" => Mode::Fixed,
                    _ => return Err(OssError::invalid_value(value, "m", "0040-00000311")),
                }
            }
            "w" => out.w = side(value, "w", "0040-00000312")?,
            "h" => out.h = side(value, "h", "0040-00000313")?,
            "l" => out.l = side(value, "l", "0040-00000314")?,
            "s" => out.s = side(value, "s", "0040-00000315")?,
            // OSS 实测 p 写错是 0317，越界（含 0）却是 0312。
            "p" => {
                out.p = match int(value) {
                    None => return Err(OssError::invalid_value(value, "p", "0040-00000317")),
                    Some(n) if (1..=1000).contains(&n) => n as u32,
                    Some(_) => return Err(OssError::invalid_value(value, "p", "0040-00000312")),
                }
            }
            "limit" => {
                out.limit = match int(value) {
                    Some(0) => false,
                    Some(n) if n > 0 => true,
                    _ => return Err(OssError::invalid_value(value, "limit", "0040-00000319")),
                }
            }
            "color" => {
                out.color = hex_color(value).ok_or_else(|| {
                    OssError::invalid_value(value, "ResizePadColor", "0040-00000318")
                })?
            }
            _ => continue,
        }
        recognized = true;
    }
    if !recognized {
        return Err(OssError::bad(
            "MissingArgument",
            "Missing argument:resize",
            Some("0040-00000322"),
        ));
    }
    if out.w == 0 && out.h == 0 && out.l == 0 && out.s == 0 && out.p == 0 {
        return Err(OssError::bad(
            "InvalidArgument",
            "Width and Height can not both be 0.",
            Some("0040-00000321"),
        ));
    }
    Ok(out)
}

/// 六位十六进制 RGB，大小写均可（OSS 实测 `FFF`、`red` 无效）。
fn hex_color(value: &str) -> Option<[u8; 3]> {
    if value.len() != 6 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&value[i..i + 2], 16).ok();
    Some([byte(0)?, byte(2)?, byte(4)?])
}

fn crop(params: &[&str]) -> Result<Crop> {
    let mut out = Crop {
        w: 0,
        h: 0,
        x: 0,
        y: 0,
        gravity: Gravity::Nw,
    };
    // 宽高：不是整数或为负是 InvalidArgument，超过 16384 是 OSS 的 CutEdgeExceedRange。
    let side = |value: &str, name: &str, ec: &'static str| -> Result<u32> {
        match int(value) {
            Some(n) if n > MAX_CROP_SIDE => Err(OssError::bad(
                "CutEdgeExceedRange",
                "Advance cut's range is invalid.",
                None,
            )),
            Some(n) if n >= 0 => Ok(n as u32),
            _ => Err(OssError::invalid_value(value, name, ec)),
        }
    };
    for (key, value) in params.iter().filter_map(|t| pair(t)) {
        match key {
            "w" => out.w = side(value, "w", "0040-00000225")?,
            "h" => out.h = side(value, "h", "0040-00000226")?,
            "x" => {
                out.x = int(value)
                    .ok_or_else(|| OssError::invalid_value(value, "x", "0040-00000223"))?
            }
            "y" => {
                out.y = int(value)
                    .ok_or_else(|| OssError::invalid_value(value, "y", "0040-00000224"))?
            }
            "g" => {
                out.gravity = match value {
                    "nw" => Gravity::Nw,
                    "north" => Gravity::North,
                    "ne" => Gravity::Ne,
                    "west" => Gravity::West,
                    "center" => Gravity::Center,
                    "east" => Gravity::East,
                    "sw" => Gravity::Sw,
                    "south" => Gravity::South,
                    "se" => Gravity::Se,
                    // g_auto / g_face 依赖 OSS 的 IMM 智能服务，网关不提供，与其他非法值同样报错。
                    _ => return Err(OssError::invalid_value(value, "g", "0040-00000227")),
                }
            }
            // OSS 实测：不认识的键被忽略。
            _ => {}
        }
    }
    Ok(out)
}

fn rotate(params: &[&str]) -> Result<u16> {
    // OSS 实测只看第一个值，后面多余的值被忽略；"agnle" 是 OSS 原文拼写。
    let Some(value) = params.first().filter(|v| !v.is_empty()) else {
        return Err(OssError::invalid_reason(
            "",
            "rotate",
            "missing agnle",
            "0040-00000216",
        ));
    };
    match int(value) {
        None => Err(OssError::invalid_reason(
            value,
            "rotate",
            "Angle must be digit",
            "0040-00000217",
        )),
        Some(n) if (0..=360).contains(&n) => Ok(n as u16),
        Some(_) => Err(OssError::invalid_value(value, "rotate", "0040-00000217")),
    }
}

/// circle / rounded-corners 的 r 参数：1–4096；缺省时 OSS 按 0 报错。
fn radius(params: &[&str], ec: &'static str) -> Result<u16> {
    let value = params
        .iter()
        .filter_map(|t| pair(t))
        .filter(|(k, _)| *k == "r")
        .map(|(_, v)| v)
        .next_back()
        .unwrap_or("0");
    match int(value) {
        Some(n) if (1..=4096).contains(&n) => Ok(n as u16),
        _ => Err(OssError::invalid_value(value, "r", ec)),
    }
}

fn indexcrop(params: &[&str]) -> Result<Op> {
    let (mut x, mut y, mut index) = (0u32, 0u32, 0u32);
    // 非负整数；OSS 对负数会返回 500，这里按参数无效处理。超出 u32 的边长等同于“比图片大”。
    let count = |value: &str, name: &str, ec: &'static str| -> Result<u32> {
        match int(value) {
            Some(n) if n >= 0 => Ok(n.min(u32::MAX as i64) as u32),
            _ => Err(OssError::invalid_value(value, name, ec)),
        }
    };
    for (key, value) in params.iter().filter_map(|t| pair(t)) {
        match key {
            "x" => x = count(value, "x", "0040-00000223")?,
            "y" => y = count(value, "y", "0040-00000224")?,
            "i" => index = count(value, "i", "0040-00000229")?,
            _ => {}
        }
    }
    if x == 0 && y == 0 {
        return Err(OssError::bad(
            "InvalidArgument",
            "X and y can not both be 0.",
            Some("0040-00000230"),
        ));
    }
    Ok(Op::IndexCrop {
        vertical: y > 0,
        size: if y > 0 { y } else { x },
        index,
    })
}

fn blur(params: &[&str]) -> Result<Op> {
    let (mut r, mut s) = (None, None);
    for (key, value) in params.iter().filter_map(|t| pair(t)) {
        match key {
            "r" => {
                r = Some((
                    value,
                    int(value)
                        .ok_or_else(|| OssError::invalid_value(value, "r", "0040-00000241"))?,
                ))
            }
            "s" => {
                s = Some((
                    value,
                    int(value)
                        .ok_or_else(|| OssError::invalid_value(value, "s", "0040-00000242"))?,
                ))
            }
            // g_face/g_faces 依赖 OSS 的 IMM 人脸检测，网关不提供。
            "g" => return Err(OssError::invalid_value(value, "g", "0040-00000227").without_ec()),
            _ => {}
        }
    }
    // OSS 先检查半径再检查标准差；缺省按 0 报错。
    let (r_text, r) = r.unwrap_or(("0", 0));
    if !(1..=50).contains(&r) {
        return Err(OssError::invalid_reason(
            r_text,
            "blur",
            "Invalid radius.",
            "0040-00000241",
        ));
    }
    // OSS 实测这里的 value 显示的是 r 的值（疑为 OSS 的笔误）；网关显示 s 本身的值，便于定位。
    let (s_text, s) = s.unwrap_or(("0", 0));
    if !(1..=50).contains(&s) {
        return Err(OssError::invalid_reason(
            s_text,
            "blur",
            "Invalid sigma.",
            "0040-00000242",
        ));
    }
    Ok(Op::Blur {
        radius: r as u8,
        sigma: s as u8,
    })
}

/// bright / contrast：单值，-100–100。
fn level(params: &[&str], name: &str) -> Result<i8> {
    let Some(value) = params.first().filter(|v| !v.is_empty()) else {
        return Err(OssError::invalid_reason(
            "",
            name,
            "missing argument",
            "0040-00000245",
        ));
    };
    match int(value) {
        None => Err(OssError::invalid_reason(
            value,
            name,
            "Value must be digit",
            "0040-00000246",
        )),
        Some(n) if (-100..=100).contains(&n) => Ok(n as i8),
        Some(_) => Err(OssError::invalid_value(value, name, "0040-00000246")),
    }
}

fn sharpen(params: &[&str]) -> Result<u16> {
    let Some(value) = params.first().filter(|v| !v.is_empty()) else {
        return Err(OssError::invalid_reason(
            "",
            "sharpen",
            "missing sharpen",
            "0040-00000279",
        ));
    };
    match int(value) {
        None => Err(OssError::invalid_reason(
            value,
            "sharpen",
            "Value must be digit",
            "0040-00000280",
        )),
        Some(n) if (50..=399).contains(&n) => Ok(n as u16),
        Some(_) => Err(OssError::invalid_value(value, "sharpen", "0040-00000280")),
    }
}

fn quality(params: &[&str]) -> Result<Quality> {
    let tokens: Vec<&str> = params.iter().copied().filter(|t| !t.is_empty()).collect();
    if tokens.is_empty() {
        return Err(OssError::invalid_reason(
            "",
            "quality",
            "missing argument",
            "0040-00000307",
        ));
    }
    let mut out = Quality {
        absolute: false,
        value: 0,
    };
    for token in tokens {
        let (absolute, value, ec) = match pair(token) {
            Some(("q", v)) => (false, v, "0040-00000309"),
            Some(("Q", v)) => (true, v, "0040-00000308"),
            // OSS 实测：quality 里出现别的参数时把整个片段当作值报错。
            _ => {
                return Err(OssError::invalid_reason(
                    token,
                    "quality",
                    "Value must be digit",
                    "0040-00000310",
                ));
            }
        };
        let name = if absolute { "Q" } else { "q" };
        out.value = match int(value) {
            None => return Err(OssError::invalid_value(value, name, ec)),
            Some(n) if (1..=100).contains(&n) => n as u8,
            Some(_) => return Err(OssError::invalid_value(value, "quality", "0040-00000310")),
        };
        out.absolute |= absolute;
    }
    Ok(out)
}

fn format(params: &[&str]) -> Result<Format> {
    // OSS 实测 format 有多个值时取最后一个（`format,png,jpg` 输出 JPEG）。
    let Some(value) = params.iter().copied().rfind(|v| !v.is_empty()) else {
        return Err(OssError::invalid_reason(
            "",
            "format",
            "missing format",
            "0040-00000207",
        ));
    };
    Format::parse(value).ok_or_else(|| OssError::invalid_value(value, "f", "0040-00000206"))
}

/// interlace 的 EC：缺值、取值无效。
const INTERLACE_EC: (&str, &str) = ("0040-00000281", "0040-00000282");
/// auto-orient 的 EC：缺值、取值无效。
const ORIENT_EC: (&str, &str) = ("0040-00000214", "0040-00000215");

/// interlace / auto-orient：单值 0 或 1，只看第一个值。
fn flag(
    params: &[&str],
    name: &str,
    (missing, invalid): (&'static str, &'static str),
) -> Result<bool> {
    let Some(value) = params.first().filter(|v| !v.is_empty()) else {
        return Err(OssError::invalid_reason(
            "",
            name,
            "missing argument",
            missing,
        ));
    };
    match int(value) {
        None => Err(OssError::invalid_reason(
            value,
            name,
            "Value must be digit",
            invalid,
        )),
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        Some(_) => Err(OssError::invalid_value(value, name, invalid)),
    }
}

impl OssError {
    /// 去掉 EC：网关自己不支持的取值没有对应的 OSS 细分码，不冒用。
    fn without_ec(mut self) -> Self {
        self.ec = None;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steps(value: &str) -> Vec<Step> {
        match parse(value).unwrap() {
            Plan::Image(steps) => steps,
            other => panic!("{value}: {other:?}"),
        }
    }
    fn one(value: &str) -> Step {
        let mut all = steps(value);
        assert_eq!(all.len(), 1, "{value}");
        all.remove(0)
    }
    fn resize_of(value: &str) -> Resize {
        match one(value).ops.as_slice() {
            [Op::Resize(r)] => *r,
            other => panic!("{value}: {other:?}"),
        }
    }
    /// 断言错误的 HTTP 状态、Code、Message 与 EC 都与 OSS 实测一致。
    fn fails(value: &str, code: &str, message: &str, ec: Option<&str>) {
        let error = parse(value).expect_err(value);
        assert_eq!(
            error.status.as_u16(),
            if code == "NoSuchStyle" { 404 } else { 400 },
            "{value}"
        );
        assert_eq!(
            (error.code, error.message.as_str(), error.ec),
            (code, message, ec),
            "{value}"
        );
    }

    #[test]
    fn easos_file_strings_parse_to_the_expected_plan() {
        for (mode, expected) in [("lfit", Mode::Lfit), ("fill", Mode::Fill)] {
            for edge in [64, 128, 256, 512, 768, 1024, 1536, 1920] {
                let step = one(&format!(
                    "image/auto-orient,1/resize,m_{mode},w_{edge},h_{edge},limit_1/quality,q_90/format,webp"
                ));
                assert!(step.auto_orient);
                assert_eq!(step.format, Some(Format::Webp));
                assert_eq!(
                    step.quality,
                    Some(Quality {
                        absolute: false,
                        value: 90
                    })
                );
                assert_eq!(step.output, Output::Image);
                assert_eq!(
                    step.ops,
                    vec![Op::Resize(Resize {
                        mode: expected,
                        w: edge,
                        h: edge,
                        l: 0,
                        s: 0,
                        p: 0,
                        limit: true,
                        color: [255, 255, 255],
                    })]
                );
            }
        }
    }

    #[test]
    fn top_level_dispatch_matches_oss() {
        // 单独的 image、空值、未知前缀都原样返回原图；style/ 没有样式可用。
        for value in [
            "image",
            "",
            "foo/bar",
            "imagex/resize,w_100",
            "video/snapshot,t_0",
        ] {
            assert_eq!(parse(value).unwrap(), Plan::Original, "{value}");
        }
        assert_eq!(parse("style/small").unwrap(), Plan::NoSuchStyle);
        fails(
            "image/",
            "MissingArgument",
            "Missing Some Required Arguments.",
            Some("0040-00000212"),
        );
        fails(
            "image//",
            "MissingArgument",
            "Missing Some Required Arguments.",
            Some("0040-00000212"),
        );
        // 空动作与结尾斜线被忽略。
        assert_eq!(steps("image/resize,w_100/").len(), 1);
        assert_eq!(
            one("image/resize,w_100//format,png").format,
            Some(Format::Png)
        );
    }

    #[test]
    fn pipes_run_in_order_and_are_bounded() {
        let all = steps("image/resize,w_100|image/format,png");
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].format, Some(Format::Png));
        // 空段、非 image 段忽略；第一段不是 image 时整条按原图。
        assert_eq!(steps("image/resize,w_100||image/format,png").len(), 2);
        assert_eq!(steps("image/resize,w_100|foo/bar").len(), 1);
        assert_eq!(steps("image/resize,w_100|sys/saveas,o_dGVzdA").len(), 1);
        assert_eq!(parse("foo/bar|image/resize,w_100").unwrap(), Plan::Original);
        let six = "image/resize,w_100|image/resize,w_90|image/resize,w_80|image/resize,w_70|image/resize,w_60|image/format,webp";
        assert_eq!(steps(six).len(), 6);
        fails(
            &format!("{six}|image/format,png"),
            "TooManyPipes",
            "Maximal number of pipes supported is 6",
            Some("0040-00000211"),
        );
    }

    #[test]
    fn unknown_and_unsupported_actions_are_invalid_process() {
        fails(
            "image/slim,1",
            "InvalidArgument",
            "The value: slim of parameter: process is invalid.",
            Some("0040-00000213"),
        );
        // 动作名大小写敏感。
        fails(
            "image/Resize,w_100",
            "InvalidArgument",
            "The value: Resize of parameter: process is invalid.",
            Some("0040-00000213"),
        );
        fails(
            "image/watermark,text_SGVsbG8",
            "InvalidArgument",
            "The value: watermark of parameter: process is invalid.",
            Some("0040-00000213"),
        );
    }

    #[test]
    fn resize_parameters_defaults_and_errors() {
        fails(
            "image/resize",
            "MissingArgument",
            "Missing argument:resize",
            Some("0040-00000322"),
        );
        fails(
            "image/resize,",
            "MissingArgument",
            "Missing argument:resize",
            Some("0040-00000322"),
        );
        // 键大小写敏感、缺下划线、未知键都视而不见。
        for value in [
            "image/resize,W_100",
            "image/resize,foo_1",
            "image/resize,w100",
        ] {
            fails(
                value,
                "MissingArgument",
                "Missing argument:resize",
                Some("0040-00000322"),
            );
        }
        for value in [
            "image/resize,w_0",
            "image/resize,w_0,h_0",
            "image/resize,s_0",
            "image/resize,l_0",
            "image/resize,m_fill",
        ] {
            fails(
                value,
                "InvalidArgument",
                "Width and Height can not both be 0.",
                Some("0040-00000321"),
            );
        }
        for (value, shown, name, ec) in [
            ("w_abc", "abc", "w", "0040-00000312"),
            ("w_-1", "-1", "w", "0040-00000312"),
            ("w_16385", "16385", "w", "0040-00000312"),
            ("w_", "", "w", "0040-00000312"),
            ("w_1e2", "1e2", "w", "0040-00000312"),
            ("w_100.0", "100.0", "w", "0040-00000312"),
            ("w_100,h_abc", "abc", "h", "0040-00000313"),
            ("l_abc", "abc", "l", "0040-00000314"),
            ("l_16385", "16385", "l", "0040-00000314"),
            ("s_abc", "abc", "s", "0040-00000315"),
            ("p_abc", "abc", "p", "0040-00000317"),
            ("p_0", "0", "p", "0040-00000312"),
            ("p_1001", "1001", "p", "0040-00000312"),
            ("m_xxx,w_100", "xxx", "m", "0040-00000311"),
            ("w_100,m_", "", "m", "0040-00000311"),
            ("w_500,limit_abc", "abc", "limit", "0040-00000319"),
            ("w_100,limit_", "", "limit", "0040-00000319"),
            ("w_100,limit_-1", "-1", "limit", "0040-00000319"),
            (
                "m_pad,w_150,h_80,color_red",
                "red",
                "ResizePadColor",
                "0040-00000318",
            ),
            (
                "m_pad,w_150,h_80,color_FFF",
                "FFF",
                "ResizePadColor",
                "0040-00000318",
            ),
        ] {
            fails(
                &format!("image/resize,{value}"),
                "InvalidArgument",
                &format!("The value: {shown} of parameter: {name} is invalid."),
                Some(ec),
            );
        }
        // 数字写法宽松；重复键取最后一个；limit 非 0 即 1；color 大小写均可。
        for value in ["w_+100", "w_0100", "w_ 100", "w_200,w_100", "h_0,w_100"] {
            assert_eq!(
                resize_of(&format!("image/resize,{value}")).w,
                100,
                "{value}"
            );
        }
        assert!(resize_of("image/resize,w_500,limit_2").limit);
        assert!(!resize_of("image/resize,w_500,limit_0").limit);
        assert_eq!(
            resize_of("image/resize,m_pad,w_1,color_ff0000").color,
            [255, 0, 0]
        );
        let r = resize_of("image/resize,w_0,l_100");
        assert_eq!((r.w, r.l, r.mode), (0, 100, Mode::Lfit));
    }

    #[test]
    fn crop_parameters_and_errors() {
        let Op::Crop(c) = one("image/crop,g_se,x_-10,y_-10,w_100,h_100,foo_1").ops[0] else {
            panic!()
        };
        assert_eq!(
            (c.w, c.h, c.x, c.y, c.gravity),
            (100, 100, -10, -10, Gravity::Se)
        );
        // 没有参数也合法：整张图。
        assert_eq!(
            one("image/crop").ops,
            vec![Op::Crop(Crop {
                w: 0,
                h: 0,
                x: 0,
                y: 0,
                gravity: Gravity::Nw
            })]
        );
        for (value, shown, name, ec) in [
            ("w_abc", "abc", "w", "0040-00000225"),
            ("w_-1", "-1", "w", "0040-00000225"),
            ("w_", "", "w", "0040-00000225"),
            ("h_abc", "abc", "h", "0040-00000226"),
            ("x_abc", "abc", "x", "0040-00000223"),
            ("y_abc", "abc", "y", "0040-00000224"),
            ("g_fac", "fac", "g", "0040-00000227"),
            ("g_auto", "auto", "g", "0040-00000227"),
            ("g_face,w_100", "face", "g", "0040-00000227"),
        ] {
            fails(
                &format!("image/crop,{value}"),
                "InvalidArgument",
                &format!("The value: {shown} of parameter: {name} is invalid."),
                Some(ec),
            );
        }
        for value in ["image/crop,w_16385", "image/crop,h_16385"] {
            fails(
                value,
                "CutEdgeExceedRange",
                "Advance cut's range is invalid.",
                None,
            );
        }
    }

    #[test]
    fn single_value_actions_follow_oss_messages() {
        assert_eq!(one("image/rotate,90").ops, vec![Op::Rotate(90)]);
        // 只看第一个值。
        assert_eq!(one("image/rotate,200,90").ops, vec![Op::Rotate(200)]);
        assert_eq!(one("image/rotate,+90").ops, vec![Op::Rotate(90)]);
        assert_eq!(one("image/rotate,360").ops, vec![Op::Rotate(360)]);
        fails(
            "image/rotate",
            "InvalidArgument",
            "The value:  of parameter: rotate is invalid.reason:missing agnle",
            Some("0040-00000216"),
        );
        fails(
            "image/rotate,abc",
            "InvalidArgument",
            "The value: abc of parameter: rotate is invalid.reason:Angle must be digit",
            Some("0040-00000217"),
        );
        fails(
            "image/rotate,90.5",
            "InvalidArgument",
            "The value: 90.5 of parameter: rotate is invalid.reason:Angle must be digit",
            Some("0040-00000217"),
        );
        fails(
            "image/rotate,-1",
            "InvalidArgument",
            "The value: -1 of parameter: rotate is invalid.",
            Some("0040-00000217"),
        );
        fails(
            "image/rotate,400",
            "InvalidArgument",
            "The value: 400 of parameter: rotate is invalid.",
            Some("0040-00000217"),
        );

        assert!(one("image/interlace,1").interlace);
        fails(
            "image/interlace",
            "InvalidArgument",
            "The value:  of parameter: interlace is invalid.reason:missing argument",
            Some("0040-00000281"),
        );
        fails(
            "image/interlace,2",
            "InvalidArgument",
            "The value: 2 of parameter: interlace is invalid.",
            Some("0040-00000282"),
        );
        fails(
            "image/interlace,abc",
            "InvalidArgument",
            "The value: abc of parameter: interlace is invalid.reason:Value must be digit",
            Some("0040-00000282"),
        );

        assert!(!one("image/auto-orient,0").auto_orient);
        // 默认转正，且与位置无关。
        assert!(one("image/resize,w_100").auto_orient);
        assert!(!one("image/resize,w_100/auto-orient,0").auto_orient);
        fails(
            "image/auto-orient",
            "InvalidArgument",
            "The value:  of parameter: auto-orient is invalid.reason:missing argument",
            Some("0040-00000214"),
        );
        fails(
            "image/auto-orient,2",
            "InvalidArgument",
            "The value: 2 of parameter: auto-orient is invalid.",
            Some("0040-00000215"),
        );
        fails(
            "image/auto-orient,abc",
            "InvalidArgument",
            "The value: abc of parameter: auto-orient is invalid.reason:Value must be digit",
            Some("0040-00000215"),
        );

        assert_eq!(one("image/bright,+50").ops, vec![Op::Bright(50)]);
        assert_eq!(one("image/contrast,-50").ops, vec![Op::Contrast(-50)]);
        fails(
            "image/bright",
            "InvalidArgument",
            "The value:  of parameter: bright is invalid.reason:missing argument",
            Some("0040-00000245"),
        );
        fails(
            "image/bright,abc",
            "InvalidArgument",
            "The value: abc of parameter: bright is invalid.reason:Value must be digit",
            Some("0040-00000246"),
        );
        fails(
            "image/bright,101",
            "InvalidArgument",
            "The value: 101 of parameter: bright is invalid.",
            Some("0040-00000246"),
        );
        fails(
            "image/bright,-101",
            "InvalidArgument",
            "The value: -101 of parameter: bright is invalid.",
            Some("0040-00000246"),
        );
        fails(
            "image/contrast",
            "InvalidArgument",
            "The value:  of parameter: contrast is invalid.reason:missing argument",
            Some("0040-00000245"),
        );
        fails(
            "image/contrast,101",
            "InvalidArgument",
            "The value: 101 of parameter: contrast is invalid.",
            Some("0040-00000246"),
        );

        assert_eq!(one("image/sharpen,100").ops, vec![Op::Sharpen(100)]);
        fails(
            "image/sharpen",
            "InvalidArgument",
            "The value:  of parameter: sharpen is invalid.reason:missing sharpen",
            Some("0040-00000279"),
        );
        fails(
            "image/sharpen,49",
            "InvalidArgument",
            "The value: 49 of parameter: sharpen is invalid.",
            Some("0040-00000280"),
        );
        fails(
            "image/sharpen,400",
            "InvalidArgument",
            "The value: 400 of parameter: sharpen is invalid.",
            Some("0040-00000280"),
        );
        fails(
            "image/sharpen,abc",
            "InvalidArgument",
            "The value: abc of parameter: sharpen is invalid.reason:Value must be digit",
            Some("0040-00000280"),
        );
    }

    #[test]
    fn quality_and_format_merge_like_oss() {
        assert_eq!(
            one("image/quality,Q_90").quality,
            Some(Quality {
                absolute: true,
                value: 90
            })
        );
        // 出现过 Q 就是绝对质量，数值取最后一个。
        assert_eq!(
            one("image/quality,Q_50,q_90").quality,
            Some(Quality {
                absolute: true,
                value: 90
            })
        );
        assert_eq!(
            one("image/quality,q_90,Q_50").quality,
            Some(Quality {
                absolute: true,
                value: 50
            })
        );
        // 多个 quality 动作取最后一个。
        assert_eq!(
            one("image/quality,q_50/quality,q_90").quality,
            Some(Quality {
                absolute: false,
                value: 90
            })
        );
        fails(
            "image/quality",
            "InvalidArgument",
            "The value:  of parameter: quality is invalid.reason:missing argument",
            Some("0040-00000307"),
        );
        fails(
            "image/quality,q_abc",
            "InvalidArgument",
            "The value: abc of parameter: q is invalid.",
            Some("0040-00000309"),
        );
        fails(
            "image/quality,Q_abc",
            "InvalidArgument",
            "The value: abc of parameter: Q is invalid.",
            Some("0040-00000308"),
        );
        fails(
            "image/quality,q_0",
            "InvalidArgument",
            "The value: 0 of parameter: quality is invalid.",
            Some("0040-00000310"),
        );
        fails(
            "image/quality,Q_0",
            "InvalidArgument",
            "The value: 0 of parameter: quality is invalid.",
            Some("0040-00000310"),
        );
        fails(
            "image/quality,q_101",
            "InvalidArgument",
            "The value: 101 of parameter: quality is invalid.",
            Some("0040-00000310"),
        );
        fails(
            "image/quality,x_1",
            "InvalidArgument",
            "The value: x_1 of parameter: quality is invalid.reason:Value must be digit",
            Some("0040-00000310"),
        );

        assert_eq!(one("image/format,jpeg").format, Some(Format::Jpg));
        assert_eq!(one("image/format,png,jpg").format, Some(Format::Jpg));
        assert_eq!(one("image/format,jpg,png").format, Some(Format::Png));
        assert_eq!(
            one("image/format,png/format,webp").format,
            Some(Format::Webp)
        );
        fails(
            "image/format",
            "InvalidArgument",
            "The value:  of parameter: format is invalid.reason:missing format",
            Some("0040-00000207"),
        );
        fails(
            "image/format,JPG",
            "InvalidArgument",
            "The value: JPG of parameter: f is invalid.",
            Some("0040-00000206"),
        );
        fails(
            "image/format,dng",
            "InvalidArgument",
            "The value: dng of parameter: f is invalid.",
            Some("0040-00000206"),
        );
    }

    #[test]
    fn effects_parameters_and_errors() {
        assert_eq!(one("image/circle,r_100").ops, vec![Op::Circle(100)]);
        fails(
            "image/circle",
            "InvalidArgument",
            "The value: 0 of parameter: r is invalid.",
            Some("0040-00000233"),
        );
        fails(
            "image/circle,r_0",
            "InvalidArgument",
            "The value: 0 of parameter: r is invalid.",
            Some("0040-00000233"),
        );
        fails(
            "image/circle,r_4097",
            "InvalidArgument",
            "The value: 4097 of parameter: r is invalid.",
            Some("0040-00000233"),
        );
        fails(
            "image/circle,r_abc",
            "InvalidArgument",
            "The value: abc of parameter: r is invalid.",
            Some("0040-00000233"),
        );
        assert_eq!(
            one("image/rounded-corners,r_30").ops,
            vec![Op::RoundedCorners(30)]
        );
        fails(
            "image/rounded-corners",
            "InvalidArgument",
            "The value: 0 of parameter: r is invalid.",
            Some("0040-00000237"),
        );
        fails(
            "image/rounded-corners,r_4097",
            "InvalidArgument",
            "The value: 4097 of parameter: r is invalid.",
            Some("0040-00000237"),
        );

        assert_eq!(
            one("image/indexcrop,x_100,i_3").ops,
            vec![Op::IndexCrop {
                vertical: false,
                size: 100,
                index: 3
            }]
        );
        // x、y 都给时 y 生效；i 缺省为 0。
        assert_eq!(
            one("image/indexcrop,x_100,y_50").ops,
            vec![Op::IndexCrop {
                vertical: true,
                size: 50,
                index: 0
            }]
        );
        fails(
            "image/indexcrop,i_0",
            "InvalidArgument",
            "X and y can not both be 0.",
            Some("0040-00000230"),
        );
        fails(
            "image/indexcrop,x_0,i_0",
            "InvalidArgument",
            "X and y can not both be 0.",
            Some("0040-00000230"),
        );
        fails(
            "image/indexcrop,x_abc,i_0",
            "InvalidArgument",
            "The value: abc of parameter: x is invalid.",
            Some("0040-00000223"),
        );
        fails(
            "image/indexcrop,x_-1,i_0",
            "InvalidArgument",
            "The value: -1 of parameter: x is invalid.",
            Some("0040-00000223"),
        );
        fails(
            "image/indexcrop,y_abc",
            "InvalidArgument",
            "The value: abc of parameter: y is invalid.",
            Some("0040-00000224"),
        );
        fails(
            "image/indexcrop,x_100,i_abc",
            "InvalidArgument",
            "The value: abc of parameter: i is invalid.",
            Some("0040-00000229"),
        );
        fails(
            "image/indexcrop,x_100,i_-1",
            "InvalidArgument",
            "The value: -1 of parameter: i is invalid.",
            Some("0040-00000229"),
        );

        assert_eq!(
            one("image/blur,r_3,s_2").ops,
            vec![Op::Blur {
                radius: 3,
                sigma: 2
            }]
        );
        fails(
            "image/blur",
            "InvalidArgument",
            "The value: 0 of parameter: blur is invalid.reason:Invalid radius.",
            Some("0040-00000241"),
        );
        fails(
            "image/blur,s_2",
            "InvalidArgument",
            "The value: 0 of parameter: blur is invalid.reason:Invalid radius.",
            Some("0040-00000241"),
        );
        fails(
            "image/blur,r_51,s_2",
            "InvalidArgument",
            "The value: 51 of parameter: blur is invalid.reason:Invalid radius.",
            Some("0040-00000241"),
        );
        fails(
            "image/blur,r_3",
            "InvalidArgument",
            "The value: 0 of parameter: blur is invalid.reason:Invalid sigma.",
            Some("0040-00000242"),
        );
        fails(
            "image/blur,r_3,s_51",
            "InvalidArgument",
            "The value: 51 of parameter: blur is invalid.reason:Invalid sigma.",
            Some("0040-00000242"),
        );
        fails(
            "image/blur,r_abc,s_2",
            "InvalidArgument",
            "The value: abc of parameter: r is invalid.",
            Some("0040-00000241"),
        );
        fails(
            "image/blur,r_3,s_abc",
            "InvalidArgument",
            "The value: abc of parameter: s is invalid.",
            Some("0040-00000242"),
        );
        fails(
            "image/blur,r_3,s_2,g_face",
            "InvalidArgument",
            "The value: face of parameter: g is invalid.",
            None,
        );
    }

    #[test]
    fn info_and_average_hue_follow_oss_precedence() {
        // info 一出现就返回源图信息：之前的动作照常校验，之后的不再解析。
        for value in [
            "image/info",
            "image/info,1",
            "image/resize,w_100/info",
            "image/info/resize,w_abc",
            "image/average-hue/info",
            "image/info/average-hue",
        ] {
            assert_eq!(one(value).output, Output::Info, "{value}");
        }
        fails(
            "image/resize,w_abc/info",
            "InvalidArgument",
            "The value: abc of parameter: w is invalid.",
            Some("0040-00000312"),
        );
        for value in [
            "image/average-hue",
            "image/average-hue,1",
            "image/resize,w_100/average-hue",
            "image/resize,w_100/average-hue/format,png",
        ] {
            assert_eq!(one(value).output, Output::AverageHue, "{value}");
        }
        fails(
            "image/average-hue/resize,w_100",
            "BadRequest",
            "Request is invalid.",
            None,
        );
        // 产出 JSON 后，管道后面的段不再执行。
        assert_eq!(steps("image/info|image/resize,w_100").len(), 1);
    }

    #[test]
    fn operations_keep_their_order() {
        assert_eq!(
            one("image/rotate,90/crop,w_100,h_50/resize,w_10").ops,
            vec![
                Op::Rotate(90),
                Op::Crop(Crop {
                    w: 100,
                    h: 50,
                    x: 0,
                    y: 0,
                    gravity: Gravity::Nw
                }),
                Op::Resize(Resize {
                    mode: Mode::Lfit,
                    w: 10,
                    h: 0,
                    l: 0,
                    s: 0,
                    p: 0,
                    limit: true,
                    color: [255, 255, 255]
                }),
            ]
        );
    }
}
