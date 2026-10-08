# 图片处理接口

## 请求与结果

`GET /?url=<百分号编码的原图地址>&w=320&h=240&q=0.9&r=2&f=webp`

该接口与 EasOS File `storage.rs::image_url` 的外部处理契约一致。File 先丢弃非法参数，再原样保留合法的 `w/h/q/r/f` 参数名和值；处理器不接收 EasOS 登录票据。无状态 `GET /healthz` 返回 `ok`，只证明进程可响应，不证明桶或网络可用。

| 字段 | 规则 |
| --- | --- |
| url | 必填；仅允许明确配置的 origin 和桶前缀。私有桶使用短期签名 GET。 |
| w / h | 1–1920，缺省各为 1920。 |
| q | 0.01–1，缺省0.9，编码时四舍五入为整数百分比；JPEG来源按量化表估算原质量后乘以q，其他来源使用绝对质量，PNG无损。 |
| r | 0：拉伸；1（缺省）：等比容纳；2：居中裁剪。按OSS默认limit=1不放大，目标需要放大时保留原尺寸（等比容纳仍可缩小）。 |
| f | jpeg、png、webp、bmp、gif、tiff、heic、avif；省略保留源格式。只有GIF输入才转GIF，其他输入f=gif保留源格式。 |
| d | 可省略或为0；d=1原文件下载由File直接重定向MinIO，不进入处理器。 |

成功返回实际图片字节、对应 Content-Type、`Cache-Control: no-store`、`Referrer-Policy: no-referrer`、`X-Content-Type-Options: nosniff` 和随机 `X-Request-Id`。HEAD 使用同一处理路径，但 HTTP 层不返回正文。未知字段、重复字段、非法参数均拒绝。

输入支持 JPEG、PNG、BMP、GIF、WebP、TIFF、HEIC、AVIF，通过魔数和原生解码器验证，不信任 Content-Type。GIF/动画WebP在输出GIF/WebP时保留帧、延迟及循环，转其他格式取首帧；多页TIFF/HEIC取首图。EXIF自动方向先于缩放，重编码移除EXIF/GPS；透明转JPEG合成白底，透明HEIC转JPEG拒绝。SVG/PDF不接入通用文档解析。

使用 libvips 原生 shrink-on-load、流式管线和 Lanczos3 缩放，WebP effort=3、HEIC fast、AVIF speed=6；HEIF输出通过libheif显式限制为单线程，避免手写像素循环或纯Rust逐像素有损编码。OSS 对照依据：[输入格式与限额](https://www.alibabacloud.com/help/zh/oss/user-guide/overview-17/)、[格式转换](https://www.alibabacloud.com/help/zh/oss/user-guide/convert-image-formats-2)、[缩放](https://www.alibabacloud.com/help/zh/oss/user-guide/resize-images-4)、[质量](https://www.alibabacloud.com/help/zh/oss/user-guide/adjust-image-quality)。对齐现有File参数覆盖的行为和格式范围，不新增OSS水印等File未定义操作，也不承诺与OSS专有编码器逐字节一致。JPEG源质量是成熟量化表估计值；不同编码器质量及压缩率可能有差异。

## 处理缓存

进程内缓存变换结果，最多256MiB/512项，LRU淘汰，寿命1小时；重启清空，按请求清理，不启动定时任务。键包含资源origin/路径、非鉴权查询（包括versionId）及规范化w/h/q/r/f，签名不同仍可复用同一结果。每次请求仍携带当前签名向MinIO发起条件GET：授权有效且强ETag未变返回304才可命中；对象更新重新编码，授权拒绝绝不返回缓存。无强ETag时不缓存。

`X-Image-Cache: HIT`表示复用编码结果，`MISS`表示本次编码并缓存，`BYPASS`表示源未提供强ETag；CORS显式暴露该头。浏览器响应仍为no-store，以免客户端缓存延长读取授权。并发首次请求可能各自编码，由全局并发上限约束。

## 存储与网络边界

`origin` 是 File 签名绑定的公开 HTTPS 地址，`upstream` 是运维设定的可信回源 origin（同服务器容器网络通常 `http://minio:9000`），`path_prefix` 是独立桶路径，例如 `/easos-preview/`。处理器保留公开 Host（含非默认端口）、编码路径和签名查询，再向固定 upstream 请求；回源协议变化不改变 S3 SigV4 的 Host/路径/查询签名。

请求者不能选择任意网络目的地，不跟随重定向、不读环境代理、不自动重试；不会透传调用者 Authorization、Cookie、Referer 或自定义请求头。路径不能跳出桶前缀或使用二次编码绕过。即使 GUI CORS 通过，也仍由上游签名决定原文件能否读取；CORS 不是身份认证。服务本身没有签名验证密钥，也不缓存已过期链接的结果。

若要连接其他 S3，把 `upstream` 设为可信的真实 HTTPS 源站，`origin` 与公开签名入口保持对应。使用不同 HTTPS 回源主机时还须验证该服务是否接受公开 Host 与不同 TLS SNI；默认部署采用同网络 MinIO HTTP 回源。前缀校验针对当前 File 使用的安全 SID 键设计，不承诺兼容包含字面百分号或点目录段的任意第三方对象名。

## 资源、失败与日志

默认输入不超过100MiB/2400万像素（动图计所有帧），单边最多30000，输出不超过20MiB。这是本服务资源预算，低于OSS部分输入配额；File参数仍限制目标边长1920。默认并行2个请求（可配1–4），下载与编码共享名额；忙时503，不排队或重试。回源总超时20秒，连接超时5秒。

每次编解码在无网络参数/凭据的一次性原生子进程中执行，地址空间768MiB、CPU25秒、墙钟30秒、输出文件20MiB限制先于解码生效。工作线程等待并回收子进程；客户端取消不会提前释放CPU名额。临时目录私有，仅在远端容器512MiB tmpfs中保存本次输入/输出，结束删除。容器另限2GiB/2CPU/128进程。预算超限返回稳定错误，不能宣称支持任意复杂度图像。

| HTTP | 原因举例 |
| --- | --- |
| 400 | missing_parameters、invalid_parameters、invalid_transform、invalid_source_url |
| 403 | source_not_allowed、source_path_not_allowed |
| 413 | source_too_large、source_pixel_limit |
| 415 | unsupported_image_format、unsupported_conversion |
| 422 | invalid_image、processing_limit |
| 502 / 504 | source_rejected / source_unreachable / source_read_failed、source_timeout |
| 503 / 500 | busy、worker_failed |

错误正文为 `{"error":{"code":"固定原因码"}}`。不返回上游错误正文或 URL。JSON 日志记录中文阶段说明、随机请求 ID、大小/尺寸、耗时和成功/失败/取消终态，不记录文件正文、对象路径、URL、签名或凭据；固定日志过滤器不接受 `RUST_LOG` 打开 HTTP 依赖调试。处理器的请求号不等于 EasOS 内部 Trace，不伪造跨浏览器重定向的父 Span；需要联调时用响应 X-Request-Id 关联本服务日志。
