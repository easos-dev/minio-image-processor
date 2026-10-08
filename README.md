# Image Processor

独立的 Rust 图片处理服务，供 EasOS File 的 MinIO / S3 外部图片处理地址调用。项目、构建和部署独立于 EasOS；不是 MOD，不需要访问 EasOS 数据库，也不持有存储 AK/SK。原图仅在远端单次请求的私有临时目录处理；处理结果使用有界内存缓存，不后台同步。

- [接口和边界](docs/protocol.md)
- [目标服务器 HTTPS 部署与 EasOS 接入](deploy/https/README.md)
- [已有开发服务器 MinIO 部署](deploy/development/README.md)（由独立部署任务维护）
- [验收记录与边界](docs/verification.md)

## 开发检查

使用 Rust 1.97.1 与 Python 3.11+。以下检查只使用临时内存图片、短生命周期 loopback HTTP 桩及临时目录，不启动 MinIO、不导入附件、不部署常驻服务。

```sh
cargo fmt --check
CARGO_BUILD_JOBS=2 cargo clippy --all-targets --locked -- -D warnings
# 需目标Linux的libvips/MagickWand和image-worker，见Dockerfile；不在Mac部署依赖
CARGO_BUILD_JOBS=2 cargo test --locked
python3 -m unittest discover -s tests -p 'test_*.py'
```

`Cargo.lock` 固定依赖；`Dockerfile` 用于目标服务器构建。配置格式见 `config.example.toml`。已删除的本地附件副本不会因本项目运行而恢复；历史图片仍需要在目标存储中有对应原文件。
