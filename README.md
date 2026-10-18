# Image Processor

MinIO/S3 前置的图片处理网关，供 EasOS File 的 MinIO/S3 下载域名使用。网关就是对象存储的公开入口：所有请求按原路径、Host、查询串（含 S3 签名）与端到端头原样转给可信上游，正文流式转发；只有 GET 带 `x-oss-process=image/...` 时取回原图，按阿里云 OSS 完全相同的参数、默认值和错误码加工后返回。网关不签名、不持凭据、不接收 EasOS 票据；授权由上游按请求自带的签名或桶策略判断。

项目、构建和部署独立于 EasOS；不是 MOD，不访问 EasOS 数据库。原图只在单次请求的私有临时目录里处理；加工结果使用有界内存缓存，不后台同步。

- [接口与 OSS 处理语义](docs/protocol.md)
- [目标服务器 HTTPS 部署与 EasOS 接入](deploy/https/README.md)
- [已有开发服务器 MinIO 部署](deploy/development/README.md)（由独立部署任务维护）
- [验收记录与边界](docs/verification.md)

## 部署（LXC / Proxmox，容器里不装 Docker）

`scripts/build-lxc-template.sh` 把运行镜像加上 systemd（`lxc/Dockerfile`）后导出成 LXC 容器模板（根文件系统 `tar.zst`），Proxmox 直接用它建容器，容器里不需要 Docker。在装有 Docker 的机器上打模板（amd64）：

```sh
scripts/build-lxc-template.sh easit2/minio-image-processor:fba75ae dist/minio-image-processor_fba75ae_amd64.tar.zst
```

Proxmox 上建容器（编号、存储、网络按实际改；MinIO 地址填进配置后重启服务）：

```sh
cp minio-image-processor_fba75ae_amd64.tar.zst /var/lib/vz/template/cache/
pct create 120 local:vztmpl/minio-image-processor_fba75ae_amd64.tar.zst --ostype debian --hostname image-processor \
  --unprivileged 1 --cores 2 --memory 4096 --rootfs local-lvm:4 --net0 name=eth0,bridge=vmbr0,ip=dhcp
pct start 120
pct exec 120 -- sed -i 's#^upstream = .*#upstream = "http://<MinIO 地址>:9000"#' /etc/image-processor/config.toml
pct exec 120 -- systemctl restart image-processor
```

- 服务 `image-processor.service` 以 nobody 运行、监听 8080，配置在 `/etc/image-processor/config.toml`（内容与 `config.example.toml` 相同）；单元文件带与 Docker 部署同等的收紧（只读根文件系统、私有 /tmp、不提权、无能力）。日志 `journalctl -u image-processor`。
- 模板里没有 sshd，用 `pct enter` / `pct exec` 进容器；网络由 Proxmox 写 `/etc/network/interfaces`（模板装了 ifupdown 与 DHCP 客户端）。
- 模板只在 Docker 里以 systemd 为 PID 1 启动核对过服务能起、能应答，没有在真实 Proxmox 上验证；首次部署后核对 `systemctl status image-processor` 与一次带 `x-oss-process` 的请求。

## 开发检查

使用 Rust 1.97.1 与 Python 3.11+。检查只使用临时内存图片、短生命周期 loopback HTTP 桩（模拟 MinIO）及临时目录，不启动常驻服务。

```sh
cargo fmt --check
CARGO_BUILD_JOBS=2 cargo clippy --all-targets --locked -- -D warnings
# 纯 Rust 单元测试：处理串解析与 OSS 错误码、转发规则、缓存、配置；Mac 上可直接跑。
CARGO_BUILD_JOBS=2 cargo test --locked --lib
python3 -m unittest discover -s tests -p 'test_*.py'
```

`tests/native.rs`、`tests/service.rs`、`tests/logging.rs` 要调用原生编码子进程，需要 Linux 的 libvips/MagickWand 与 `image-worker`，用 Dockerfile 的构建阶段跑，不在 Mac 安装这些依赖：

```sh
docker build --target build -t image-processor-build .
docker run --rm -v "$PWD":/src:ro -w /src -e CARGO_TARGET_DIR=/tmp/target \
  image-processor-build cargo test --locked
```

`Cargo.lock` 固定依赖；`Dockerfile` 用于目标服务器构建。配置格式见 `config.example.toml`。已删除的本地附件副本不会因本项目运行而恢复；历史图片仍需要在目标存储中有对应原文件。
