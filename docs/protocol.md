# 网关接口与 OSS 图片处理语义

## 定位

网关就是 MinIO（或其他 S3 兼容存储）的公开入口，部署在可信上游前面：所有请求按原样转给配置的 `upstream`（同网络通常 `http://minio:9000`）；只有 GET 带 `x-oss-process=image/...` 时，网关取回原图、按阿里云 OSS 的参数语义加工后返回。网关不签名、不验签、不持存储凭据，也不接收 EasOS 票据；读写授权一律由上游按请求自带的签名或桶策略判断。

EasOS File 对三种存储拼同一条 OSS 处理串（`image/auto-orient,1/resize,m_lfit|m_fill,w_N,h_N,limit_1/quality,q_90/format,webp`，N 为 64–1920 的档位）。OSS 原生处理；MinIO/S3 的下载域名指向本网关，参数、默认值、错误码与 OSS 一致。

参数与行为以 [OSS 图片处理文档](https://www.alibabacloud.com/help/en/oss/user-guide/image-processing) 为准；文档没写或与线上不一致的细节，以 OSS 官方公开示例桶的实测应答为准（2026-10-18 实测，见文末“对照记录”）。

## 请求分流

| 请求 | 网关行为 |
| --- | --- |
| GET，`x-oss-process` 以 `image/` 开头 | 加工（见“加工流程”） |
| GET，`x-oss-process` 为 `image`、空值或不认识的前缀（`foo/…`、`video/…` 等） | 原样返回原对象（OSS 对未知前缀同样返回原图） |
| GET，`x-oss-process=style/<名称>` | 404 `NoSuchStyle`：网关没有样式配置 |
| HEAD（不论是否带处理串） | 透传，返回原对象的头；OSS 实测 HEAD 带处理串同样不加工 |
| 其他方法（PUT 直传、分片上传、DELETE、OPTIONS 等） | 透传，查询串一律不动 |

参数名大小写敏感，只认小写 `x-oss-process`；出现多次时取第一个（OSS 实测）。值先做百分号解码，`image%2Fresize%2Cw_100` 与 `image/resize,w_100` 等价。

## 透传规则

- 请求路径、查询串（含 `X-Amz-*` 签名参数）、Host 与端到端请求头逐字转给上游；`Authorization`、`Range`、条件头、`Content-MD5`、`x-amz-*` 都原样转。去掉 HTTP 规定代理不得转发的逐跳头（`Connection` 及其点名的头、`Keep-Alive`、`Transfer-Encoding`、`TE`、`Trailer`、`Upgrade`、`Proxy-*`）和 `Expect`（100-continue 由网关自己应答；S3 SDK 不把它签进 SigV4）。
- 请求正文按原 `Content-Length` 流式转发；客户端分块上传时上游同样收到分块正文；没有正文的请求不附加正文。
- 响应的状态码、端到端头（ETag、Content-Length、Content-Range、Accept-Ranges、Cache-Control、Content-Disposition、x-amz-* 等）与正文原样流式回给客户端，不缓冲整个对象；Range/206、HEAD、304、412、上游错误 XML 都如此。网关只追加 `X-Request-Id`。
- 不跟随重定向、不重试、不读环境代理、不加默认请求头。透传路径不设总时限（大文件可以慢慢传），只有 5 秒建连时限。
- 上游地址由 `url` 解析：已有的百分号编码保持不变；只会补编码裸露的非法字符，并按 URL 规范消解 `.`/`..` 路径段（MinIO 本就拒绝这类对象名）。

### 转发前是否去掉 `x-oss-process`

- **匿名 GET/HEAD**（没有 `X-Amz-Signature`/`Signature` 查询参数，也没有 `Authorization` 头）：转发前去掉所有 `x-oss-process`，其余查询片段逐字节保留原顺序。EasOS 已发布对象的公开地址属于这种。
- **带签名的请求**：查询串整条原样转给上游。SigV4 的规范化查询包含全部参数，签名方（EasOS File 的私有读取地址）把处理串一并签进了 URL，删掉它 MinIO 就会验签失败。这与 OSS 的规则一致：OSS 私有读取同样要求把 `x-oss-process` 签进 URL，签名后再追加处理串会被拒（网关场景下由 MinIO 返回 403 `SignatureDoesNotMatch`）。MinIO 对 GetObject 不认识的查询参数只参与验签、不影响取对象，所以上游照样返回原图，由网关加工。

## 加工流程

1. 先解析处理串；参数错误在回源前直接返回 OSS 错误（因此带错参数的请求不先验签，与 OSS“先鉴权后解析”的顺序不同，只影响错误码的先后）。
2. 排队取得编码名额，排队期间不回源。
3. 带客户端的鉴权与条件头向上游 GET 原图，去掉 `Range`/`If-Range`（OSS 对处理请求同样忽略 Range，总是返回完整结果）。客户端没带 `If-None-Match`/`If-Modified-Since` 而缓存里有结果时，附上缓存的 ETag 做 `If-None-Match`。
4. 按上游应答：
   - 304 且是网关自己的条件请求：缓存命中（`X-Image-Cache: HIT`）。
   - 200 且 ETag 与缓存相同：缓存命中，不读原图正文。
   - 200：读原图（超过 20 MB 即停，`ImageTooLarge`），按魔数识别格式（不信任 Content-Type），编码；有强 ETag 时写入缓存（`MISS`），否则不缓存（`BYPASS`）。
   - 其他（客户端条件请求的 304、403、404、412、5xx 等）：原样透传，不加工、不报 OSS 错误。授权被拒的请求绝不会拿到缓存内容。
5. 加工结果的响应头：上游本次应答的端到端头，去掉描述原图正文的头（Content-Length、Content-Type、Content-Range、Accept-Ranges、Content-Encoding、Content-MD5、x-amz-checksum-*、x-amz-mp-parts-count）；缓存命中时用缓存里记下的 Cache-Control、Content-Disposition、Content-Language、Expires、ETag、Last-Modified、x-amz-version-id、x-amz-meta-* 补齐上游 304 没带的头；再加上结果的 Content-Type、Content-Length 与 `X-Image-Cache`。ETag 沿用原对象的 ETag（OSS FAQ：处理结果的 ETag 与原对象相同，不随处理串变化），所以客户端带它做条件请求时，由上游判断对象是否变化即可得到正确的 304。

## 处理串语法

- 形如 `image/<动作>,<参数>,<参数>/<动作>…`；动作按串里的先后执行。动作名、参数键、取值都大小写敏感（OSS 实测 `Resize`、`W_100`、`format,JPG` 都不生效）。
- 空动作（`//`、结尾的 `/`、`resize,w_100,` 的空参数）忽略。键值参数写作 `key_value`；同一动作里重复的键取最后一个；resize/crop 不认识的键忽略。
- 整数写法宽松：允许前后空白、`+`/`-` 号和前导零；小数、科学计数法、空串都不是整数。
- 管道：`|` 分隔最多 6 段（超过返回 `TooManyPipes`），每段都是 `image/...`，前一段的编码结果是后一段的输入；非 `image/` 的后续段（如 GET 上的 `sys/saveas`）忽略，第一段不是 `image` 时整条按原图返回。
- `info` 一出现就返回本段输入的信息：它前面的动作照常校验，后面的动作不再解析。`average-hue` 之后若还有像素操作，返回 `BadRequest`；之后只有 format/quality 等设置时照常返回平均色。
- `auto-orient`、`format`、`quality`、`interlace` 是本段的设置，与位置无关，多次出现取最后一次。`auto-orient` 默认 1，在解码时就决定，作用于本段所有操作之前。

## 支持的操作

| 动作 | 参数（默认值） | 说明 |
| --- | --- | --- |
| `resize` | `m`：lfit（默认）/mfit/fill/pad/fixed；`w`、`h`、`l`、`s`：0–16384，0 为未指定；`p`：1–1000；`limit`：0 或其他非负整数（默认 1）；`color`：6 位十六进制（默认 FFFFFF） | 见下方缩放规则 |
| `crop` | `w`、`h`：0–16384，0 为到边界；`x`、`y`：整数，可负；`g`：nw（默认）/north/ne/west/center/east/sw/south/se | 锚点按 OSS 九宫格表（如 center 为 `W/2-w/2, H/2-h/2`）加偏移；起点在图外报 `CutEdgeExceedRange`，越过左上边界按 0，区域超出图片时截到边界 |
| `rotate` | 0–360，只看第一个值 | 顺时针；非直角时画布扩成外接矩形（`floor(W·|cos|+H·|sin|)`，400×300 转 45° 得 494×494），新出现的角不透明图填白、带透明通道的图填透明；图片任一边超过 4096 时报错 |
| `circle` | `r`：1–4096 | 以中心裁出 `2r+1` 见方的内切圆；r 超过短边一半时取 `(短边-1)/2`；圆外透明，输出 JPEG 时为白色 |
| `rounded-corners` | `r`：1–4096 | r 超过短边一半时取短边一半；角外透明，JPEG 为白色 |
| `indexcrop` | `x` 或 `y`：≥1（都给时 y 生效）；`i`：≥0（默认 0） | 按边长切块取第 i 块；边长超过图片或序号超过块数时返回原图 |
| `blur` | `r`：1–50；`s`：1–50（都必填） | 高斯模糊，标准差 s，核在半径 r 处截断 |
| `bright` | -100–100 | 颜色通道整体加 v% 满幅 |
| `contrast` | -100–100 | 绕中灰 128 拉伸 (1+v%) 倍 |
| `sharpen` | 50–399 | libvips 锐化，强度 m2=v/50 |
| `quality` | `q` 或 `Q`：1–100 | 见质量规则 |
| `format` | jpg、jpeg、png、webp、bmp、gif、tiff、heic、avif；多个值取最后一个 | gif 只对 GIF 源生效，其他源保持原格式；带透明的 HEIC 不能存成 JPG；其他透明图转 JPG 合成白底 |
| `interlace` | 0 或 1 | 1 输出渐进 JPEG；非 JPEG 忽略 |
| `auto-orient` | 0 或 1（默认 1） | 按 EXIF Orientation 转正 |
| `info` | 无 | JSON：FileSize、Format、FrameCount、ImageWidth、ImageHeight（原始未转正尺寸），图片自带分辨率时加 ResolutionUnit/XResolution/YResolution，非 PNG 再加 EXIF 字段；版式与 OSS 逐字节相同（键按字母序、四空格缩进、末尾 `}}`） |
| `average-hue` | 无 | JSON `{"RGB": "0xRRGGBB"}`，取本段输入（忽略它前面的操作）第一帧白底合成后的平均色 |

不提供：`watermark`（需要字体与同桶水印对象）、crop 的 `g_auto`/`g_face` 与 blur 的 `g_face`/`g_faces`（依赖 OSS 的 IMM 智能服务）、样式（`style/`）、`sys/saveas` 等持久化处理和 video/doc 处理。不支持的动作按 OSS 对未知动作的方式报 `InvalidArgument`（EC 0040-00000213），不支持的 g 取值报 `InvalidArgument`。

### 缩放规则

- 给了 w 或 h 时由 m 决定，l/s/p 忽略：
  - lfit：等比缩放到放得进 w×h 的最大尺寸；mfit：等比缩放到盖满 w×h 的最小尺寸；fill：按 mfit 缩放后居中裁成 w×h；pad：按 lfit 缩放后居中放在 w×h 的 color 底色上；fixed：强制拉伸到 w×h。
  - 只给一边时：lfit/mfit/fixed 按这一边等比缩放；fill/pad 按边长为该值的正方形处理。
- 没给 w/h、同时给 l 与 s 时：按图片方向换成宽高框（横图 w=l、h=s），再按 m 处理（OSS 实测 fixed 照样强制拉伸）；只给 l 或 s 时按长边/短边等比缩放，m 不起作用。
- 只给 p 时按百分比缩放，不受 limit 约束（OSS 实测 `p_120` 直接放大）。
- `limit` 默认 1：需要放大时（lfit/mfit/fill 的缩放比大于 1，fixed 的任一目标边大于原图）返回原尺寸，后面的格式、质量照常生效；`limit_0` 允许放大。pad 不受 limit 约束，内图照样放大再填充（OSS 实测）。
- 计算出的小数尺寸四舍五入。缩放结果（含 fill 的中间图）单边超过 16384 或总像素超过 16777216 时报错。
- w、h、l、s 全为 0 且没有 p 时：没有任何可识别参数报 `MissingArgument`，否则报 `Width and Height can not both be 0.`。

### 质量规则（OSS 实测）

源图质量：JPEG 用 ImageMagick 的量化表估算，其他格式按 95。

| 输出 | 不写 quality | `q_N`（相对） | `Q_N`（绝对） |
| --- | --- | --- | --- |
| JPEG | 源图质量 | floor(源图质量 × N / 100) | min(源图质量, N) |
| WebP / HEIC / AVIF | min(源图质量, 80) | floor(源图质量 × N / 100) | N |
| PNG / BMP / GIF / TIFF | 不适用（无损或不分档） | 忽略 | 忽略 |

同一个 quality 里只要出现过 `Q` 就按绝对质量，数值取最后一个（OSS 实测 `Q_50,q_90` 与 `q_90,Q_50` 分别得 90 与 50 的绝对质量）。EasOS 的 `quality,q_90/format,webp` 对 JPEG 源得到源图质量的 90%，对 PNG 源得到 85。OSS 文档说 WebP 的 q 等同绝对质量，但线上实测是相对质量，网关按线上行为。

### 动图

GIF/WebP 多帧图只有在操作全是 resize/crop 且输出 GIF/WebP 时保持多帧（帧延迟与循环保留）；其他操作或输出静态格式时只取第一帧（OSS：动图只有缩放、裁剪、水印保持动态，旋转等让动图变静态）。动图的像素总数按 宽×高×帧数 计。

### 通用效果

所有输出都去掉 EXIF/GPS 等元数据（OSS 实测处理结果不带 EXIF）；带 ICC 配置文件的图先转到 sRGB 再去元数据，免得 Display P3 等照片偏色。JPEG 用 optimize_coding；WebP effort 3；HEIC/AVIF 由 libheif 单线程编码（x265 fast / AOM speed 6）。

## 限额（OSS 的数值）

| 项目 | 上限 | 超出时 |
| --- | --- | --- |
| 源图大小 | 20 MB | `ImageTooLarge`，EC 0040-00000001 |
| 源图单边 | 30000（TIFF 16384、AVIF 10000） | `ImageTooLarge`，EC 按格式（JPEG 0019、BMP 0017、TIFF 0018、HEIC 0013、AVIF 0014、其他 0009） |
| 源图总像素（动图乘帧数） | 2.5 亿 | `ImageTooLarge`，EC 0040-00000012 |
| 旋转的图 | 单边 4096 | `ImageTooLarge`，`Picture exceed the maximum allowable rotation range.`，EC 0040-00000006 |
| 缩放结果 | 单边 16384、总像素 16777216 | `BadRequest`，EC 0040-00000007 |
| WebP 输出 | 单边 16383 | `BadRequest`，EC 0040-00000011 |
| HEIC 输出 | 单边 4096 | `BadRequest`，EC 0040-00000014 |
| AVIF 输出 | 宽 4096、总像素 4096×2304 | `BadRequest`，EC 0040-00000014 |

## 错误

网关自己返回的错误是 OSS 同结构的 XML（`Content-Type: application/xml`，有 EC 时另带 `x-oss-ec` 头），`RequestId` 与响应头 `X-Request-Id` 相同，`HostId` 是请求的 Host：

```xml
<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>InvalidArgument</Code>
  <Message>The value: abc of parameter: w is invalid.</Message>
  <RequestId>…</RequestId>
  <HostId>bucket.example.com</HostId>
  <EC>0040-00000312</EC>
</Error>
```

处理串错误的 HTTP 状态、Code、Message 原文与 EC 逐条照 OSS 实测（单元测试 `src/process.rs` 列出了全部用例），常见几类：

| 情形 | HTTP | Code | EC |
| --- | --- | --- | --- |
| 参数值无效：`The value: <值> of parameter: <参数> is invalid.`（rotate、quality、interlace 等缺值或非数字时在句号后接 `reason:…`） | 400 | InvalidArgument | 按动作与参数，如 resize w 0312、h 0313、l 0314、s 0315、p 0317/0312、m 0311、color 0318、limit 0319；crop w 0225、h 0226、x 0223、y 0224、g 0227；rotate 0216/0217；quality 0307–0310；format 0206/0207；interlace 0281/0282；auto-orient 0214/0215；circle 0233；rounded-corners 0237；indexcrop x 0223、y 0224、i 0229；blur r 0241、s 0242；bright/contrast 0245/0246；sharpen 0279/0280 |
| 不认识或不提供的动作：`The value: <动作> of parameter: process is invalid.` | 400 | InvalidArgument | 0040-00000213 |
| `image/` 后没有动作 | 400 | MissingArgument | 0040-00000212 |
| resize 没有可识别参数：`Missing argument:resize` | 400 | MissingArgument | 0040-00000322 |
| resize 边长全为 0：`Width and Height can not both be 0.` | 400 | InvalidArgument | 0040-00000321 |
| indexcrop 的 x、y 都为 0：`X and y can not both be 0.` | 400 | InvalidArgument | 0040-00000230 |
| crop 宽高超过 16384：`Advance cut's range is invalid.`；起点在图外：`Advance cut's position is out of image.` | 400 | CutEdgeExceedRange | 无 |
| 管道超过 6 段：`Maximal number of pipes supported is 6` | 400 | TooManyPipes | 0040-00000211 |
| average-hue 之后还有像素操作：`Request is invalid.` | 400 | BadRequest | 无 |
| `style/…` | 404 | NoSuchStyle | 无 |
| 对象不是可识别的图片：`This image format is not supported.` | 400 | BadRequest | 0040-00000005 |
| 图片损坏 | 400 | BadRequest（WebP 为 BadWebPImage） | 0040-00000002（WebP 0010、HEIC 0020、AVIF 0021） |
| 带透明的 HEIC 存成 JPG | 400 | BadRequest | 无 |
| 单次处理超出子进程内存/时间预算 | 400 | ImageTooLarge | 无 |

网关特有的失败没有 OSS 对应项，用 S3/OSS 客户端都认识的码，不给 EC：

| 情形 | HTTP | Code |
| --- | --- | --- |
| 连不上上游或读上游中途断开 | 502 | InternalError |
| 处理路径回源超时、处理请求整体超过 55 秒 | 504 | InternalError |
| 排队位已满（正在编码之外再有 256 个等待） | 503 | ServiceUnavailable |
| 排队超过 45 秒 | 503 | ServiceUnavailable |
| 编码子进程异常 | 500 | InternalError |
| 请求 URI 无法解析 | 400 | InvalidURI |

上游自己的错误（签名不对、对象不存在、条件不满足等）不经过这里，状态码、头和正文原样透传。

## 与 OSS 的已知差异

- 不提供 watermark、IMM 智能裁剪/人脸模糊、样式与持久化处理（见上）。
- average-hue：OSS 的算法是“突出颜色”的平均（FAQ 原文，未公开），网关取简单平均；实测纯色图结果相同，普通照片会有差别（示例图 OSS 0xbbcd7f、网关 0xc7c08a）。
- info 的 EXIF 字段名取 libexif 标签名，值取原始值（数字、有理数、ASCII），与 OSS 的写法可能不同（如 OSS 把 GPSLatitude 写成 `0deg `，网关是 `0/1 0/1 0/1`）；ExifTag、GPSTag 这类 IFD 偏移指针不返回。OSS 文档说 info 不支持 AVIF，网关也返回。
- 缩放结果按文档限制在 16384 边长与 16777216 像素；OSS 线上实测 10000×10000 也能返回。
- OSS 文档说 rotate 等单值动作给多个值会报错（EC 0040-00000203），线上实测只取第一个值，网关按线上；同理 info 与其他动作同用时线上直接返回 info。
- OSS 对 indexcrop 的负数 i 返回 500，网关按参数无效（EC 0040-00000229）返回 400。blur 标准差越界时 OSS 的 Message 显示的是 r 的值，网关显示 s 本身的值。
- 动图的 p 缩放与放大 OSS 不支持，网关照常处理；rounded-corners 对 GIF OSS 不支持，网关取第一帧处理。
- 源图解码失败、超限、输出超限这几类错误的 Code/Message 原文 OSS 没有公开，也无法在示例桶上复现；EC 取自 OSS 错误中心，Code 取 OSS 错误码表中语义对应的一项，Message 由网关撰写。
- 编码器不同，结果字节、文件大小与 OSS 不一致；JPEG 源质量是量化表估计值。

## 处理缓存

进程内缓存加工结果，最多 256 MiB / 512 项，单条超过 32 MiB 不缓存，LRU 淘汰，寿命 1 小时；重启清空，过期与淘汰只在请求里顺带进行，不启动定时任务。键为 Host + 原始对象路径 + 非鉴权查询（解码后排序，含 versionId、response-* 等；不含签名参数与 `x-oss-process`）+ 规范化处理串（解析后的参数序列，同一语义的不同写法得到同一个键），签名不同仍可复用同一结果。每次请求仍带当前签名向上游发条件 GET：授权有效且强 ETag 未变才命中；对象更新重新编码；授权被拒绝不返回缓存。弱 ETag 或没有 ETag 时不缓存。

`X-Image-Cache: HIT` 表示复用编码结果，`MISS` 表示本次编码并缓存，`BYPASS` 表示上游没有强 ETag；只出现在加工结果上。部署者自查网关是否生效：对同一对象用两条不同的处理串各取一次，长度不同、且带 `X-Image-Cache` 即生效。并发的首次请求可能各自编码，由全局并发上限约束。

## 资源、并发与日志

源图上限见“限额”。默认 `max_concurrent=0`，启动时按实际可用 CPU 核数与容器内存预算自动计算并发；也可指定 1–128，但仍受内存预算约束。预算按每个编码任务 1 GiB、缓存与运行时预留 512 MiB 计算；不足时保留最少 1 个。回源与编码共享名额，额外最多 256 个处理请求按到达顺序等待；等待期间不下载原图、不另发请求，取消立即退出队列。等待最多 45 秒，处理请求整体最多 55 秒（含等待、回源、编码），在前置 Nginx 60 秒时限前返回明确错误；处理路径回源总时限 `fetch_timeout_seconds`（默认 20 秒），建连 5 秒，无自动重试。透传请求不占编码名额、不排队。

每段处理在无网络参数、无凭据的一次性原生子进程中执行，地址空间 768 MiB、CPU 25 秒、墙钟 30 秒、输出文件 80 MiB 的限制先于解码生效；libvips 随机访问解码改在内存里进行（`VIPS_DISC_THRESHOLD=512m`），不写临时文件。工作线程等待并回收子进程；客户端取消不会提前释放名额。临时目录私有，只保存本段输入与输出，结束删除。超出预算返回稳定错误，不能宣称支持任意复杂度图像。

JSON 日志记录中文阶段说明、随机请求号、方法、透传或加工、HTTP 状态、错误 Code/EC/原因、大小/尺寸、耗时、排队毫秒数和成功/失败/取消终态；不记录路径、查询串、Host、签名、请求头或正文；固定日志过滤器不接受 `RUST_LOG` 打开 HTTP 依赖调试。请求号不等于 EasOS 内部 Trace，联调时用响应 `X-Request-Id` 关联本服务日志。存活检查经网关打到上游自己的健康端点（MinIO 为 `/minio/health/live`），网关不占用任何路径做自己的健康端点。

## 对照记录

2026-10-18 用 OSS 官方公开示例桶（`oss-console-img-demo-cn-hangzhou` 的 example.jpg 400×300 与 panda.png、`image-demo` 的 f.jpg 424×333 带 Orientation=7）实测了上述语法、默认值、取值范围、错误码、质量档位与响应头（处理结果沿用原 ETag/Last-Modified、不带 Accept-Ranges/Content-MD5；Range 被忽略；HEAD 不加工；带原 ETag 的 If-None-Match 得 304；If-Match 不符得 412）。随后把同字节的三张图放进本地 MinIO，经网关逐条比对 184 个处理串的状态码、错误 Code/EC、输出类型与像素尺寸：183 条一致，唯一不同的是网关不提供的 watermark。比对脚本只发 GET，不进仓库。
