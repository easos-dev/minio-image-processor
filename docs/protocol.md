# 图片处理接口

## 请求与结果

`GET /?url=<百分号编码的原图地址>&width=320&height=240&quality=90&fit=cover&format=webp`

该接口与 EasOS File `storage.rs::image_url` 的外部处理契约一致。File 的 `w/h/q/r/f` 由 File 转为下面的完整字段；处理器不接收 EasOS 登录票据。无状态 `GET /healthz` 返回 `ok`，只证明进程可响应，不证明桶或网络可用。

| 字段 | 规则 |
| --- | --- |
| url | 必填；仅允许明确配置的 origin 和桶前缀。私有桶使用短期签名 GET。 |
| width / height | 1–1920，缺省各为 1920。 |
| quality | 整数 1–100，缺省 90。JPEG/WebP 实际使用该质量；PNG 为无损。 |
| fit | contain（缺省）：等比容纳且不放大；cover：居中裁剪后缩放到目标；fill：拉伸到目标。 |
| format | jpg（兼容 jpeg）、png、webp；省略保留源格式。 |

成功返回实际图片字节、对应 Content-Type、`Cache-Control: no-store`、`Referrer-Policy: no-referrer`、`X-Content-Type-Options: nosniff` 和随机 `X-Request-Id`。HEAD 使用同一处理路径，但 HTTP 层不返回正文。未知字段、重复字段、非法参数均拒绝。

仅支持静态 JPEG、PNG、WebP，读取真实文件格式，不信任远端 Content-Type。EXIF 方向在缩放前应用；重编码移除 EXIF/GPS。JPEG 的透明区域合成白底；PNG/WebP 保留透明度。GIF、SVG、HEIF、AVIF 与动画 PNG/WebP 不在当前范围，明确返回错误，不能将本实现当作视频或文档预览器。

## 存储与网络边界

`origin` 是 File 签名绑定的公开 HTTPS 地址，`upstream` 是运维设定的可信回源 origin（同服务器容器网络通常 `http://minio:9000`），`path_prefix` 是独立桶路径，例如 `/easos-preview/`。处理器保留公开 Host（含非默认端口）、编码路径和签名查询，再向固定 upstream 请求；回源协议变化不改变 S3 SigV4 的 Host/路径/查询签名。

请求者不能选择任意网络目的地，不跟随重定向、不读环境代理、不自动重试；不会透传调用者 Authorization、Cookie、Referer 或自定义请求头。路径不能跳出桶前缀或使用二次编码绕过。即使 GUI CORS 通过，也仍由上游签名决定原文件能否读取；CORS 不是身份认证。服务本身没有签名验证密钥，也不缓存已过期链接的结果。

若要连接其他 S3，把 `upstream` 设为可信的真实 HTTPS 源站，`origin` 与公开签名入口保持对应。使用不同 HTTPS 回源主机时还须验证该服务是否接受公开 Host 与不同 TLS SNI；默认部署采用同网络 MinIO HTTP 回源。前缀校验针对当前 File 使用的安全 SID 键设计，不承诺兼容包含字面百分号或点目录段的任意第三方对象名。

## 资源、失败与日志

默认输入不超过 100 MiB / 2400 万像素，单边最多 32768，解码器分配预算 256 MiB，输出不超过 20 MiB。先读取头与预算再解码；cover 先裁原图，fill 还限制纵向采样的浮点中间像素数。默认并行 2 个请求（可配 1–4），下载和 CPU 编码共享名额；忙时返回 503，不排队或后台重试。回源总超时默认 20 秒，连接超时 5 秒。编解码工作在阻塞线程，已经开始的 CPU 工作不能强行中断；取消后名额直到该工作结束才释放。容器资源限制仍是最后一道边界，不承诺恶意编解码输入永不耗尽 CPU。

| HTTP | 原因举例 |
| --- | --- |
| 400 | missing_parameters、invalid_parameters、invalid_transform、invalid_source_url |
| 403 | source_not_allowed、source_path_not_allowed |
| 413 | source_too_large、source_pixel_limit、intermediate_pixel_limit、output_too_large |
| 415 | unsupported_image_format、animated_image_not_supported |
| 422 | invalid_image、image_encode_failed |
| 502 / 504 | source_rejected / source_unreachable / source_read_failed、source_timeout |
| 503 / 500 | busy、worker_failed |

错误正文为 `{"error":{"code":"固定原因码"}}`。不返回上游错误正文或 URL。JSON 日志记录中文阶段说明、随机请求 ID、大小/尺寸、耗时和成功/失败/取消终态，不记录文件正文、对象路径、URL、签名或凭据；固定日志过滤器不接受 `RUST_LOG` 打开 HTTP 依赖调试。处理器的请求号不等于 EasOS 内部 Trace，不伪造跨浏览器重定向的父 Span；需要联调时用响应 X-Request-Id 关联本服务日志。
