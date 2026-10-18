# Nginx HTTPS 部署与接入

项目部署于 `33.79.0.35:/opt/image-processor`。全部构建和常驻服务在该服务器运行，Mac 仅保留源码。现有 MinIO 位于 `/opt/image-processor-dev`；本目录复用 `image-processor-dev` Docker 网络和 `minio:9000`，不重建其容器或数据卷。

| 入口 | 链路 |
| --- | --- |
| `https://33.79.0.35:443` | Nginx（TLS、CORS、只开放专用桶）→ `image-processor:8080`（原样转发；GET 带 `x-oss-process` 时加工）→ `minio:9000` |

只有这一个对外入口：EasOS 的控制、上传、下载域名都填它。443 仅开放指定桶，管理 API、Console、其他桶和其他路径返回 404。Nginx 与网关都保留原始 Host、路径和查询，S3 SigV4 签名在 MinIO 处校验；网关不持凭据、不签名。

## 首次部署

以下命令在目标 Linux 项目目录执行，使用现有账户，不新建系统账户。服务器需有 Docker Compose 和 Python 3.11+。先核对 443 空闲、现有 MinIO 网络及根凭据目录。

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

处理器使用镜像内 nobody 身份、只读文件系统与资源限制；图片临时文件仅放在 1 GiB tmpfs，缓存为有界内存；Nginx 的临时目录也使用 tmpfs。默认 `max_concurrent=0`，按 CPU 及内存预算自动选择并发，额外的处理请求在当前 HTTP 连接中有界等待；当前 8 核/8 GiB 主机给处理器 6 GiB，自动得到 5 个并发。透传（含上传）不占编码名额。没有业务轮询、同步、自动重试或重启循环。`docker compose stop` 仅停止本目录服务；不删除共享网络或 MinIO 卷。

存活检查经网关打到 MinIO 自己的健康端点：`curl -fsS http://image-processor:8080/minio/health/live`（同网络容器内），同时证明网关与上游都可用；网关不占用任何对象路径做自己的健康端点。

网关和 Nginx 都不记录含签名的 URI；启动配置错误仍可见。处理器保留脱敏阶段和终态日志，不输出路径、Host 或凭据。固定 MinIO 社区版本的桶 CORS 是空实现，因此 CORS 由 Nginx 精确管理：只允许配置的 GUI origin、GET/HEAD/PUT 和 EasOS File 需要的请求头，暴露 ETag、Content-Range、X-Image-Cache 等，并清除上游宽泛 CORS。对象自带的 Cache-Control、Content-Disposition 原样到达浏览器，入口不改写。generated/cors.xml 仅供支持桶 CORS 的其他 S3 服务参考。

### 从 8443 旧版升级

旧版把图片处理放在单独的 8443 端口，接口是 `/?url=…&w=…`；EasOS 已改为三种存储拼同一条 OSS 处理串、下载域名即网关入口，旧接口已删除。已有部署按下面替换，不重跑 bootstrap、不动 MinIO：

1. `generated/config.toml` 改成 `config.example.toml` 的结构（`listen`、`upstream = "http://minio:9000"`、`max_concurrent`、`fetch_timeout_seconds`），旧的 `sources`、`allowed_origins`、`max_input_*` 会让新版拒绝启动。
2. `generated/nginx.conf` 按 `scripts/prepare.py` 当前生成的内容改为单个 443：专用桶的两个 location 改为 `proxy_pass http://image-processor:8080`，删除 8443 server 和强制 `Cache-Control: no-store`，`client_max_body_size 0`，CORS 请求头与暴露头补齐。可在空目录运行一次 prepare.py 取得模板，凭据仍用现有 secrets。
3. 拉取新代码后 `docker compose build image-processor && docker compose up -d image-processor gateway`，再 `docker compose run --rm --no-deps --entrypoint nginx gateway -t` 校验配置。
4. EasOS 侧：桶配置删掉图片处理地址（EasOS 升级时自动清除），下载域名保持 `33.79.0.35`。

### 不带签名的读取

网关是无状态的转换层，不持任何凭据与授权，请求带着自己的签名原样转给 MinIO。所以能不能不带签名读，完全由 MinIO 桶本身的权限决定：桶是公开读时，不带签名经网关就能读；桶是私有时只能带签名读，和直连 MinIO 一样。EasOS File 不设置、不修改任何桶或对象的 ACL（MinIO 也不支持对象级 ACL，`PUT ?acl` 返回 501），读不到就是读不到，File 的“测试连接”会照实报错。EasOS 的媒体地址是不带签名的稳定地址，所以要让 EasOS 在 MinIO 上显示图片，需要部署者自己决定把桶（或对象前缀）设成公开读；bootstrap 不替部署者做这件事。

## HTTPS 与真实 S3 验收

```sh
# 项目根目录；测试对象使用随机前缀并在结束时清理。
python3 -m venv .venv
.venv/bin/pip install -r scripts/requirements-smoke.txt
.venv/bin/python scripts/smoke.py \
  --storage https://33.79.0.35 \
  --bucket easos-preview --gui-origin http://127.0.0.1:18080 \
  --access-key-file deploy/https/secrets/access_key \
  --secret-key-file deploy/https/secrets/secret_key \
  --ca-file deploy/https/tls/ca.crt
```

经网关检查条件 PUT/412、MD5 拒绝、签名读取与上传字节一致、Range/HEAD/304、匿名与篡改签名拒绝、允许和拒绝来源的 CORS、管理路径隔离；EasOS 的两条处理串各取两次（尺寸不同、`X-Image-Cache` 先 MISS 后 HIT、ETag 沿用原对象）；签名后再追加处理串被拒；OSS 参数错误码。已给某前缀配好匿名只读时加 `--public-prefix <前缀>/` 核对匿名读取与加工。TLS 始终验证证书链、有效期和主机/IP；企业根 CA 缺少 KeyUsage，Python helper 使用常规 TLS 客户端验证，不额外启用 Python 3.13 的严格 X.509 扩展检查，不跳过证书验证。

## 独立 EasOS 配置

仅配置 `http://127.0.0.1:18080` 独立预览，原 8080 环境保持不变。File 使用系统 CA 根，需安装带 `rustls-tls-native-roots` 的版本，并在对应工作树运行：

```sh
python3 scripts/preview.py trust-ca --certificate /Users/pch18/Codes/ca/root/root-public.crt
```

只把公开根 CA 放进容器信任库，私钥留在远端 Nginx。预览脚本保留根 CA 副本，重建后重新安装。

File 桶 key 为 `minio_preview`，provider=MinIO、bucket=easos-preview、region=us-east-1、path_style=true；server/upload/download 的 host 均为 `33.79.0.35`、HTTPS 开启。host 不含协议或路径。没有单独的图片处理地址：MinIO 的下载域名就是网关入口。凭据取专用应用文件，在安全通道配置，不写入文档、聊天或命令日志。示例见 `generated/easos-bucket.example.toml`。

默认桶和已有模块的新上传分配到新桶。历史记录保留原 bucket_key，旧桶配置不能删除或改类型/桶名。验收范围见 [验收记录](../../docs/verification.md)。
