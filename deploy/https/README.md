# Nginx HTTPS 部署与接入

项目部署于 `33.79.0.35:/opt/image-processor`。全部构建和常驻服务在该服务器运行，Mac 仅保留源码。现有 MinIO 位于 `/opt/image-processor-dev`；本目录复用 `image-processor-dev` Docker 网络和 `minio:9000`，不重建其容器或数据卷。

| 服务 | 对外入口 | Nginx 回源 |
| --- | --- | --- |
| MinIO S3 | `https://33.79.0.35:443` | `minio:9000` |
| 图片处理 | `https://33.79.0.35:8443` | `image-processor:8080` |

两个监听均使用企业 CA 签发、SAN 包含该 IP 的服务证书。443 仅开放指定桶，8443 仅开放处理入口 `/` 和 `/healthz`；管理 API、Console、其他桶和其他路径返回 404。Nginx 保留原始 Host、路径和查询，确保 S3 SigV4 有效。

## 首次部署

以下命令在目标 Linux 项目目录执行，使用现有账户，不新建系统账户。服务器需有 Docker Compose 和 Python 3.11+。先核对 443/8443 空闲、现有 MinIO 网络及根凭据目录。

```sh
python3 scripts/prepare.py --host 33.79.0.35 \
  --bucket easos-preview --gui-origin http://127.0.0.1:18080 \
  --minio-secrets-dir /opt/image-processor-dev/secrets \
  --minio-network image-processor-dev
```

将服务证书、匹配私钥和根 CA 安全放入 `deploy/https/tls/server.crt`、`server.key`、`ca.crt`。目录 0700、私钥 0600，全部属部署账户，不能进入 Git 或镜像。证书续期由运维替换文件并重载 Nginx，不使用公共 CA 自动签发。

```sh
cd deploy/https
docker compose config --quiet
docker compose build image-processor bootstrap
docker compose run --rm --no-deps --entrypoint nginx gateway -t
docker compose --profile setup run --rm --no-deps bootstrap
docker compose up -d image-processor gateway
```

生成器拒绝覆盖已有 generated/secrets/.env。应用 AK/SK 为独立凭据，权限仅限专用私有桶，文件 0600；根凭据只挂载给一次性 bootstrap，不能交给 EasOS。已有同名桶使 bootstrap 停止，避免接管数据；中断后检查阶段并手动续做，禁止删除凭据再初始化。已有部署只审查并更新配置、重建所需服务，不再次 bootstrap。

处理器使用镜像内 nobody 身份、只读文件系统与资源限制；Nginx 的临时目录使用 tmpfs。没有业务轮询、同步、自动重试或重启循环。`docker compose stop` 仅停止本目录服务；不删除共享网络或 MinIO 卷。

网关不记录含签名 URI 的 access/error 日志；启动配置错误仍可见。处理器保留脱敏阶段和终态日志，不输出源地址或凭据。固定 MinIO 社区版本的桶 CORS 是空实现，因此 CORS 由 Nginx 精确管理：只允许配置的 GUI origin、GET/HEAD/PUT 和必要请求头，并清除上游宽泛 CORS。generated/cors.xml 仅供支持该功能的其他 S3 服务参考。

## HTTPS 与真实 S3 验收

```sh
# 项目根目录；测试对象使用随机前缀并在结束时清理。
python3 -m venv .venv
.venv/bin/pip install -r scripts/requirements-smoke.txt
.venv/bin/python scripts/smoke.py \
  --storage https://33.79.0.35 --processor https://33.79.0.35:8443 \
  --bucket easos-preview --gui-origin http://127.0.0.1:18080 \
  --access-key-file deploy/https/secrets/access_key \
  --secret-key-file deploy/https/secrets/secret_key \
  --ca-file deploy/https/tls/ca.crt
```

实测条件 PUT/412、MD5 拒绝、HEAD/GET 一致性、匿名/篡改签名拒绝、允许和拒绝来源的 CORS、管理路径隔离、9 组图片转换和任意来源拒绝。TLS 始终验证证书链、有效期和主机/IP；企业根 CA 缺少 KeyUsage，Python helper 使用常规 TLS 客户端验证，不额外启用 Python 3.13 的严格 X.509 扩展检查，不使用跳过证书验证。

## 独立 EasOS 配置

仅配置 `http://127.0.0.1:18080` 独立预览，原 8080 环境保持不变。File 使用系统 CA 根，需安装带 `rustls-tls-native-roots` 的版本，并在对应工作树运行：

```sh
python3 scripts/preview.py trust-ca --certificate /Users/pch18/Codes/ca/root/root-public.crt
```

只把公开根 CA 放进容器信任库，私钥留在远端 Nginx。预览脚本保留根 CA 副本，重建后重新安装。

File 新桶 key 为 `minio_preview`，provider=MinIO、bucket=easos-preview、region=us-east-1、path_style=true；server/upload/download 的 host 均为 `33.79.0.35`、HTTPS 开启，image_processor host 为 `33.79.0.35:8443`、HTTPS 开启。host 不含协议或路径，加速地址留空。凭据取专用应用文件，在安全通道配置，不写入文档、聊天或命令日志。

默认桶和已有模块的新上传分配到新桶。历史记录保留原 bucket_key，旧桶配置不能删除或改类型/桶名。此前约 18.6 GB 本地附件副本按要求删除，业务数据库保留；未复制的旧图不会因切换新桶恢复。验收范围见 [验收记录](../../docs/verification.md)。
