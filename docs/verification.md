# 开发与部署验收

2026-10-08，在独立项目与授权服务器完成以下验证。常驻 MinIO、Nginx 和图片处理均在 `33.79.0.35`，没有在 Mac 新增这些服务。EasOS 修改仅位于独立工作树和 18080 预览。

## 已通过

- 原 Rust 实现的 fmt、clippy 和 12 项测试通过：格式/质量、缩放、EXIF、像素预算、固定来源与 Host/路径/查询保留、重定向/超时、不重试、并发/取消及日志脱敏。本轮未改 Rust 处理实现。
- 当前 2 项 Python 生成器测试通过，覆盖输入校验、两端口配置、权限与拒绝覆盖秘密。旧 Caddy 测试已删除，不再计作 Nginx 证据。
- Linux amd64 实际 release 镜像构建成功。Nginx 1.30.5、Rust 处理器实际启动；Compose 配置与 nginx -t 通过。镜像摘要固定，构建在服务器执行。
- 复用现有 MinIO，初始化独立私有桶 easos-preview 与最小桶权限应用凭据；MinIO 容器 ID 和启动时间前后相同。
- 两端口 HTTPS 使用企业根 CA 验证。Mac 与 EasOS 容器均能验证证书链和 IP；未关闭证书校验。443 的图片健康路径被拒绝，8443 的 /healthz 返回 ok。
- 远端真实 S3 smoke 通过：条件上传、重复 PUT 412、MD5 失败拒绝、HEAD/GET 字节一致、匿名/篡改签名拒绝、精确 CORS、管理/其他桶隔离、JPEG/PNG/WebP × contain/cover/fill 共 9 组转换、任意来源拒绝。测试对象清理成功。
- 独立 EasOS 的 File 已安装系统 CA 版本，运行二进制 SHA256 与构建一致。MinIO 与通用 S3 两种草稿 config_test 均通过 upload_write、upload_overwrite_guard、server_head、server_read、download_read、cleanup。
- File 新增 minio_preview，默认及已有模块的新上传分配到该桶；原桶和历史记录归属保留。业务数据库保留，此前约 18.6 GB 本地附件副本已按要求删除。

File 配置页面已实际点击“测试连接”，显示“连接测试成功”；截图保存在 `.checks/file-connection-success.png`，地址页面核对 443 存储与 8443 外部处理分别生效。

源检查输出与不含凭据的运行摘要存于本地 .checks。独立 EasOS 使用 minio-enterprise-tls-20261008 质量任务封存；秘密只在服务端私有配置、原配置的 0600 运维备份中，不进入 Git。

## 未覆盖边界

内置浏览器访问两个远端 IP 入口时返回 ERR_BLOCKED_BY_CLIENT，未绕过浏览器限制。因此命令行 CORS、File 服务端连接测试及配置页面可见，不能替代浏览器直接 PUT、图片显示、下载与完整业务表单验收。需在允许该服务器访问的实际浏览器补齐此路径。

没有迁入旧附件字节，12234 条历史 File 记录不会因切换新桶自动恢复图片；完整相册、Safari/iPad 触控、独立 CDN、任意其他 S3 实现、跨桶迁移与压力测试不在本次通过范围。deploy/development 属于另一任务维护，保留其文件与 MinIO 数据。
