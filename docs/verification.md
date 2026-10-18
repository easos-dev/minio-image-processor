# 开发与部署验收

本项目独立于 EasOS。下面记录当前版本（MinIO 前置、与 OSS 同参数的图片处理网关）实际做过的检查与没覆盖的边界；旧版 `/?url=` 处理器的远端验收记录见 Git 历史中本文件的早期版本。

## 本机检查（2026-10-18）

- Mac（arm64）：`cargo fmt --check`、`cargo clippy --all-targets --locked -- -D warnings` 通过；31 项纯 Rust 单元测试通过（处理串解析与全部 OSS 错误码/EC/Message 用例、XML 错误版式、转发规则与逐跳头、签名识别与缓存键、info JSON 逐字节版式、缓存 LRU、准入队列、配置校验）；2 项部署生成器测试通过。
- 生产镜像构建：`docker build` 按 Dockerfile 在 bookworm（libvips 8.14.1、libheif 1.15.1、ImageMagick 6.9.11）完成，C 子进程以 `-Wall -Wextra -Werror` 编译通过。本机是 arm64，目标服务器的 amd64 构建未在本机做。
- 在上述构建阶段跑 `cargo test --locked`：52 项全部通过——31 项单元、9 项原生（tests/native.rs：对照 OSS 实测的 70 余条几何尺寸、顺时针旋转与白/透明角、九种格式与 Content-Type、透明合成、OSS 质量规则、EXIF 默认转正与去除、动图帧保留、info/average-hue、源图与输出限额及损坏图片的错误映射）、11 项 HTTP（tests/service.rs：loopback MinIO 桩下透传的路径/查询串/签名逐字、Host 与端到端头、逐跳头不转发、Range/206、HEAD、304、篡改签名 403、3 MiB 直传按 Content-Length 流式转发、分块上传、其他方法的查询串不动；匿名去掉 x-oss-process、签名请求保留；EasOS 两条处理串的尺寸、MISS→HIT、ETag 与对象头沿用、304 时对象头由缓存补齐、等价写法同键、处理请求忽略 Range、对象更新重编码、弱 ETag BYPASS；客户端条件请求由上游判断；上游 403/404 原样透传不重试；参数错误回源前返回；非图片与超 20 MB；回源超时与上游不可达；排队取消释放名额；24 张并发不超预算）、1 项日志（成功、上游 404、参数错误、排队取消四种终态成对，日志不含签名、Host、路径、处理串）。
- 同样的 52 项也在 trixie 测试环境（libvips 8.16.1、libheif 1.23.4、ImageMagick 7.1.1）通过；C 子进程同时兼容 ImageMagick 6/7 的头文件路径。

## 真实 MinIO 联调（本机 Docker，2026-10-18）

MinIO 由官方源码提交 `9e49d5e7a648f00e26f2246f4dc28e6b07f8c84a`（RELEASE.2025-10-15，与开发服务器同一版本）构建，网关用上面构建的生产镜像，按部署参数运行（只读根文件系统、1 GiB tmpfs、6 GiB 内存、128 进程、去掉全部能力），三者共用 loopback。全部数据为随机临时对象，结束清理。

- `scripts/smoke.py --direct --public-prefix public/` 全部阶段通过：经网关的预签名条件 PUT 与重复 412、MD5 拒绝；预签名 GET 与上传内容、与直连 MinIO 逐字节一致；Range 206、单独签名的 HEAD、If-None-Match 304、匿名与篡改签名 403；把 x-oss-process 签进 URL 后 EasOS 两条处理串分别得到 256×192 与 64×64 的 WebP（长度不同），换签名再取为 HIT 且字节一致，ETag 沿用原对象；签名后再追加处理串被 MinIO 以签名不符拒绝，匿名读私有对象 403；OSS 参数错误码；匿名前缀下不带参数返回原图、带参数返回加工结果。
- OSS 对照：OSS 官方公开示例图（example.jpg、f.jpg、panda.png）的同字节副本放进 MinIO，184 条处理串分别发给 OSS 与网关，比对状态码、错误 Code/EC、输出类型和像素尺寸，183 条一致；唯一不同是网关不提供的 watermark。average-hue 算法不同，数值不比（见 [接口文档](protocol.md#与-oss-的已知差异)）。
- MinIO 健康端点经网关返回 200；网关近千行日志里没有签名参数、桶名、上游地址或处理串。
- MinIO 对 `PUT ?acl`（`x-amz-acl: public-read`）返回 501 `NotImplemented`。这与网关无关：EasOS File 不设置任何 ACL（2026-10-18 起），能否不带签名读取由 MinIO 桶本身的权限决定，见 [部署说明](../deploy/https/README.md#不带签名的读取)。

## 未覆盖边界

- 远端 `33.79.0.35` 仍运行旧版处理器（8443 端口、`/?url=` 接口）。本次没有部署、没有改远端 Nginx，也没有重跑远端 HTTPS smoke 与 CORS 检查；升级步骤见部署说明。远端 MinIO、历史照片迁入的对象（12228 条记录对应 6831 个去重文件）未受影响。
- 未做 EasOS 完整链路（File 301 → 网关）、浏览器显示、独立 CDN、AWS S3 等其他上游，以及持续高负载压力测试。
- HEIC/AVIF 只验证了小图编码与回读；接近 4096 边长的 HEIC/AVIF 编码在 768 MiB 子进程预算内能否完成未实测。
- 未在示例桶上复现的 OSS 错误（源图超限、损坏、输出超限）其 Code/Message 由网关撰写，EC 取自 OSS 错误中心。

源码测试、联调与对照的过程输出未进 Git；原图、签名与凭据不进入仓库。
