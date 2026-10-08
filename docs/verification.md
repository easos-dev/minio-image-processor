# 开发验收

2026-10-08，在独立项目 `/Users/pch18/Codes/git.easit.jp/image-processor` 验证。没有改动 EasOS 源码或 8080 原环境，没有在 Mac 部署 MinIO/图片处理常驻服务。本任务也没有访问或变更已有远端服务器。

## 已通过

- Rust 1.97.1，macOS arm64：`cargo fmt --check`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo test --locked`。
- 12 项 Rust 测试：JPEG/PNG/WebP 编解码及质量、三种缩放、透明白底/alpha、EXIF 方向及元数据移除、长图裁剪及中间像素限制、APNG 拒绝、非法参数和输入预算、公开 Host/编码路径/查询保留、来源与路径拒绝、CORS、上游重定向/拒绝/超时、不重试、并发忙碌、取消释放以及成功/失败/取消日志关联与脱敏。
- 3 项 Python 测试：部署参数校验、权限和不覆盖秘密；已有 Caddy 2.10.2 运行短生命周期 HTTP 桩，实测双来源 CORS、PUT 预检、移除上游通配与 credentials、保留签名 Host/路径、阻止访问其他桶和 MinIO 管理路径。测试进程、监听与临时目录在结束时清理。
- Docker Compose 配置解析、bootstrap shell 语法、Python 编译与 smoke CLI 参数解析。
- Rust/Go/Debian/Caddy 镜像的远端 manifest 查询成功，Dockerfile/Compose 固定摘要；没有为此拉取镜像或执行本机 Docker 构建。
- 独立 EasOS 本地附件副本所在对象容器/卷已按用户要求删除，禁用自动随预览启动。业务数据库和独立预览入口保留；现存 `deploy/development/**` 属于另一部署任务，本任务保留原内容。

详细命令输出保存在本地 `.checks/tests.log`、`.checks/python-tests.log`、`.checks/clippy.log` 等，不包含业务数据或真实凭据。临时生成的测试配置不用于部署。

## 部署后必须完成

- Linux release 镜像的实际构建和启动、Caddy 2.11.2 目标版本配置/运行、DNS/公开 TLS、MinIO 专用桶和应用权限初始化。这些不能由本机编译或 Caddy 2.10.2 桩测试替代。
- 执行 `scripts/smoke.py`，实际验证 S3 条件上传、MD5、读写签名与字节、CORS、图片处理和临时对象回收。本次仅检查该脚本语法/CLI，未执行真实 S3 联调，不把 HTTP 桩当作 S3 兼容性通过。
- 独立 EasOS File 自身“测试连接”，再用真实浏览器完成上传、缩略图、裁剪、原图下载、错误和删除链路，确认 CSP/CORS 与各调用者可达性。
- 根据需要另行迁入历史附件，保持原 SID/存储归属；只有数据库记录无法恢复未复制的原图。

以上待验收边界均不影响开发代码与部署材料交付；服务器接入完成前，图片功能仍不能标为已恢复。
