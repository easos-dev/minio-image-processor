# 目标服务器 HTTPS 接入

本目录是部署准备，尚未执行图片处理服务部署。所有构建、初始化和 `compose up` 命令都在指定 Linux 服务器执行；Mac 只保留源代码和隔离测试。已有 `deploy/development` 维护 MinIO；本目录加入其现有 Docker 网络，不操作它的 Compose、容器或数据卷。

## 前置条件

- 服务器已有 Docker Compose、Python 3.11+，MinIO 容器在明确的网络内可用，网络别名为 `minio:9000`。
- 两个独立域名指向服务器：对象存储与图片处理。80/443 可达且未被其他服务占用；已有网关时把生成的两个站点配置合入该网关，不抢占端口。
- 已有 MinIO 根凭据文件目录仅用于一次性初始化；新的业务桶由独立 EasOS 开发实例独占，禁止复用原环境桶。根凭据不填入 EasOS。
- 固定 MinIO `RELEASE.2025-10-15T17-29-55Z` 来自[官方源码](https://github.com/minio/minio/releases/tag/RELEASE.2025-10-15T17-29-55Z)。[社区仓库已归档](https://github.com/minio/minio)，此部署用于开发验证；版本与长期运维安排需要在实际服务器上线时核对。

## 生成和检查（服务器执行）

将完整项目放入服务器现有账户可写的独立目录；不创建 Linux 用户或组。用真实域名替换示例，以下路径是原开发服务器目录约定，换机器须相应调整：

```sh
python3 scripts/prepare.py \
  --storage-domain objects.example.com \
  --image-domain images.example.com \
  --bucket easos-preview \
  --gui-origin http://127.0.0.1:18080 \
  --minio-secrets-dir /opt/image-processor-dev/secrets \
  --minio-network image-processor-dev
cd deploy/https
docker compose config --quiet
docker compose build image-processor bootstrap
# 只校验 Caddy 配置；不会启动对外监听。
docker compose run --rm --no-deps gateway caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile
```

生成器拒绝覆盖现有 generated/secrets/.env。`secrets/` 为 0700，应用 AK/SK 文件为 0600；配置和示例不含秘密。配置变更直接审查现有文件，不删除秘密目录再重生成。不要将密钥放入命令、聊天、Git 或构建上下文。

## 初始化和启动（服务器执行）

确认专用桶尚不存在，再执行一次：

```sh
docker compose --profile setup run --rm --no-deps bootstrap
docker compose up -d image-processor gateway
```

初始化仅创建该桶、应用用户及该桶的 Get/Put/Delete/List 策略，不复制附件、不设置匿名访问。已有同名桶会停止初始化，避免接管其他实例的数据。中途失败时保留已创建资源及原凭据，先检查失败阶段，再在服务器内按 `bootstrap.sh` 的未完成步骤手动继续，不自动删除或重复初始化。处理服务用镜像内既有 nobody 身份、只读文件系统和内存/CPU/进程数限制；根凭据只挂载到一次性 bootstrap。

网关仅暴露指定桶路径与图片处理入口；MinIO 管理接口/Console 不在公网代理范围。Caddy 保留 S3 原始 Host/路径/查询，不开启请求访问日志或可能包含签名查询的 HTTP 错误日志，TLS 和进程日志保留。不要额外打开代理调试、访问日志或 MinIO 请求审计转储。

固定社区版本的桶 CORS 是[空实现](https://github.com/minio/minio/blob/9e49d5e7a648f00e26f2246f4dc28e6b07f8c84a/cmd/api-router.go#L460)，因此不调用 `mc cors set` 冒充验证成功。公开 HTTPS 网关覆盖 CORS，只允许明确的 GUI origin、GET/HEAD/PUT、content-type/content-md5/if-none-match，并清除上游宽泛的 CORS 响应。`generated/cors.xml` 只供真正支持桶 CORS 的其他 S3 服务参考。源存储与处理器都需要通过浏览器 CORS 实测。

默认没有重启循环、业务轮询、自动同步或重试；TLS 证书生命周期由 Caddy 的 HTTPS 机制管理。停止本目录服务用 `docker compose stop`，不会停止独立 MinIO；不要删除其他项目的网络或数据卷。更新处理器只重建/替换本服务，不重启 EasOS 或 MinIO。

## S3 / 图片验收

在服务器中安装一次性测试依赖并执行；脚本只使用一个随机对象前缀，失败/取消也尝试删除：

```sh
# 在项目根目录执行；所有网络连接仍指向实际 HTTPS 入口。
python3 -m venv .venv
.venv/bin/pip install -r scripts/requirements-smoke.txt
.venv/bin/python scripts/smoke.py \
  --storage https://objects.example.com \
  --processor https://images.example.com \
  --bucket easos-preview \
  --gui-origin http://127.0.0.1:18080 \
  --access-key-file deploy/https/secrets/access_key \
  --secret-key-file deploy/https/secrets/secret_key
```

覆盖 SigV4 path-style 预签名 PUT、禁止覆盖 412、MD5 拒绝、HEAD/GET 字节核对、匿名/篡改签名拒绝、CORS 预检、9 组图片格式/缩放和任意来源拒绝。输出只含固定检查项、终态和耗时；清理失败明确给出该次随机测试前缀以便人工回收。该脚本不证明所有 S3 实现可靠，更不代替 EasOS 本身的签名与真实浏览器验收。

## 接入 EasOS 独立预览

等目标服务验收完成，再仅在独立开发实例 File 设置中添加新桶，参照 `generated/easos-bucket.example.toml`：

- provider=MinIO（同一 S3 适配器也可另以 S3 配置验证），region=us-east-1，path_style=true。
- server/upload/download 均填浏览器和 EasOS 容器都可达的同一 HTTPS 存储域名，确保 File 识别为直连并签发短期 GET；EasOS 在 Mac 时不要填写服务器 Docker 网络内部名字。
- 外部图片处理填 HTTPS 图片域名；host 只填域名，不含协议或路径。加速地址留空。
- AK/SK 使用专用应用凭据；在 File 点击“测试连接”，检查普通 PUT/禁止覆盖/HEAD/GET/删除，再手动设置需要开发的模块分配。

新增桶或修改模块分配不会移动旧文件，也不能改变历史记录引用的旧桶类型/名称。旧图片要恢复，需要以后明确安排从原环境读取并迁入相应原文件；本次不复制附件、不把 12234 条历史记录改指空桶、不共享原环境写凭据。已删除约 18.6 GB 本地附件副本，业务数据库保留。

最后通过 `http://127.0.0.1:18080` 的实际 File 上传小测试图片：验证浏览器 OPTIONS/PUT、原图下载、缩略图、居中裁剪、窄窗显示和错误后手动重试；确认最终请求走两个 HTTPS 域名且无 CSP/CORS 错误。用 File 删除本次测试文件，并保留请求 ID 与不含签名的验收摘要。服务器地址、DNS/TLS 和该实际链路未完成前，不宣称图片问题已经修复。
