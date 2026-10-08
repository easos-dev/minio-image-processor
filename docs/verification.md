# 开发与部署验收

本项目独立于 EasOS。当前常驻服务在 `33.79.0.35`：MinIO经Nginx 443，图片处理经Nginx 8443；Mac没有部署这些常驻服务。EasOS改动及运行验收只涉及独立工作树 `easos-codex` 和 `easos-preview-codex`（18080）。

## 当前处理协议与代码验证

2026-10-08 的短参数/格式/缓存版本完成以下检查：

- Rust fmt、clippy（全部target、warnings拒绝）通过；C worker以 `-Wall -Wextra -Werror` 编译通过。
- 目标Linux release下13项Rust测试全部通过（12项服务/变换、1项实际日志）：JPEG/PNG/BMP/GIF/WebP/TIFF/HEIC/AVIF输入输出、GIF与动画WebP帧预算、EXIF、透明、质量、三种缩放、不放大、SSRF/来源及签名Host保留、超时/并发/取消、缓存条件校验及安全日志。整组服务测试6.83秒。
- 2项部署生成器测试通过；Nginx配置显式暴露X-Image-Cache，原生成器已有的两端口、秘密权限与拒绝覆盖检查保留。
- EasOS `image-parity-20261008` 的13项质量门禁封存并验证；File全生命周期用例在独立PostgreSQL新建的专属临时数据库通过，结束后删除该数据库。夹具改为使用显式配置及manifest权限，移除了固定共享端口/容器依赖。

## 已构建、安装与真实链路

- 远端Linux amd64生产镜像构建、启动成功；只替换图片处理器，Nginx校验配置并重载。MinIO容器ID和启动时间前后相同。
- 独立EasOS通过Kernel包升级安装File，保留配置和数据。安装二进制SHA256与构建产物一致：`2673ceca95b72d2e70f7c7de438da1346eb6b0c9309a251100345da3228368e9`。
- 企业CA验证保持开启。真实S3 smoke通过条件PUT/重复412、MD5失败拒绝、HEAD/GET一致、匿名/篡改签名拒绝、精确CORS、管理路径及其他桶隔离、7种静态输出×3种缩放的21组转换和缓存二次命中；HEIC/AVIF解码尺寸由原生往返测试验证，smoke不把无法用Pillow解码当作通过。随机smoke对象已物理清理。
- 完整File实际路径通过：公开SID请求返回302；合法w/h/q/r/f保留，非法值/未知字段/外来url丢弃；处理器从MinIO读取，第二次返回HIT且字节一致；d=1直接到MinIO原文件。
- 隔离Chrome真实打开File地址，经302后解码显示128×64 WebP，HTTP200、X-Image-Cache=HIT。另核对File清理预览返回的远端PNG，浏览器显示320×200（原图较小，默认不放大）。没有关闭TLS验证或使用用户共享调试连接。
- File链路测试只创建本任务随机目录的临时图片，结束通过File逻辑删除；物理字节遵循File现有PUT保护期和显式清理流程。上一轮取消的Chrome草稿同样保持过期，未提交业务订单。

## 性能样本

同一台服务器、1200万像素JPEG噪声样图、目标800×600。旧处理器WebP两次约1125/732ms，新版首次460ms、缓存命中12ms；JPEG旧版654/644ms，新版首次304ms、命中12ms。包含HTTPS及回源；当前JPEG来源的q还按OSS相对质量处理，与旧版绝对质量不同，因此这些是端到端样本，不是固定画质的编码器基准或压力测试。

此前HEIC在容器内按宿主机核数分配x265线程池导致超时；改为libheif显式单线程编码后格式回归通过。WebP采用原生libwebp、libvips缩小解码及有界缓存；没有手写编解码算法。

## 保留边界

对齐的是当前File短参数所覆盖的OSS语义和八种格式范围，不包含File没有定义的水印、旋转角度等全部OSS操作，不保证与OSS专有编码器逐字节相同。JPEG源质量为成熟量化表估计。默认输入预算2400万像素，低于OSS部分配额；HEIF输出采用8位像素，具体资源限制见[接口和边界](protocol.md)。缓存为进程内256MiB/512项、1小时，重启清空；命中仍校验当前签名和ETag。

Chrome图片链路已通过，但本轮未重做完整业务表单、Safari/iPad触控、独立CDN、任意其他S3或并发压力验收。历史附件未迁入，旧File记录不会因切换新桶恢复图片。`deploy/development`及其MinIO数据属于另一任务，未修改。

源码测试、smoke、性能及浏览器的脱敏结果保存在 `.checks/`，截图在 `output/playwright/remote-image-chrome.png`；这些不是发布资产。原图/签名/凭据不进入Git。
