//! 配置文件（TOML）：监听地址、可信上游与资源预算。
//!
//! 网关不持有任何存储凭据；上游是运维指定的固定地址（同网络 MinIO），请求者无法改变回源目的地。
//! 源图大小与像素上限是 OSS 的固定数值（见 transform.rs / native/worker.c），不做成配置项。

use serde::Deserialize;
use std::{net::SocketAddr, time::Duration};
use url::Url;

/// 网关配置。
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// 监听地址，如 `0.0.0.0:8080`。
    pub listen: SocketAddr,
    /// 可信上游 origin（只含协议、主机、端口），同网络部署通常为 `http://minio:9000`。
    pub upstream: String,
    /// 同时编码的任务数；0 表示按可用 CPU 与容器内存自动计算，1–128 为显式值（仍受内存预算约束）。
    #[serde(default)]
    pub max_concurrent: usize,
    /// 处理路径回源（取原图）的总时限，秒，1–60。透传路径不设总时限，大文件可以慢慢传。
    #[serde(default = "fetch_timeout")]
    pub fetch_timeout_seconds: u64,
}

/// 回源默认 20 秒：20 MB 源图在同网络内远低于此值，又给 55 秒的整体处理时限留出编码时间。
fn fetch_timeout() -> u64 {
    20
}

/// 校验 origin：只能是无凭据、无路径/查询/片段的 http(s) 地址。
pub fn origin(value: &str) -> std::result::Result<Url, &'static str> {
    let url = Url::parse(value).map_err(|_| "地址格式无效")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.origin().ascii_serialization() != value
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("上游必须是无凭据、路径、查询及片段的 HTTP(S) origin");
    }
    Ok(url)
}

impl Config {
    /// 启动前校验；任何一项不合要求都拒绝启动。
    pub fn validate(&self) -> std::result::Result<(), &'static str> {
        origin(&self.upstream)?;
        if self.max_concurrent > 128 || !(1..=60).contains(&self.fetch_timeout_seconds) {
            return Err("资源限制超出范围");
        }
        Ok(())
    }

    /// 实际编码并发：0 按可用 CPU 自动选择；显式值也受内存上限约束，避免容器 OOM 影响整页。
    pub fn effective_concurrency(&self) -> usize {
        let cpus = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1);
        concurrency(self.max_concurrent, cpus, memory_budget())
    }

    /// 处理路径的回源总时限。
    pub fn fetch_timeout(&self) -> Duration {
        Duration::from_secs(self.fetch_timeout_seconds)
    }
}

/// 每个编码任务按 1 GiB 计（子进程 768 MiB 地址空间 + 原图缓冲与 tmpfs），另留 512 MiB 给缓存与运行时；
/// 内存不足时仍保留 1 个任务。
fn concurrency(configured: usize, cpus: usize, memory: u64) -> usize {
    let memory_slots = memory.saturating_sub(512 * 1024 * 1024) / (1024 * 1024 * 1024);
    let requested = if configured == 0 { cpus } else { configured };
    requested.clamp(1, 128).min(memory_slots.max(1) as usize)
}

/// 可用内存：物理内存与 cgroup 限额取小；都读不到时按 2 GiB 保守估计。
fn memory_budget() -> u64 {
    let physical = std::fs::read_to_string("/proc/meminfo").ok().and_then(|s| {
        s.lines().find_map(|line| {
            line.strip_prefix("MemTotal:")
                .and_then(|v| v.split_whitespace().next())
                .and_then(|v| v.parse::<u64>().ok())
                .and_then(|kb| kb.checked_mul(1024))
        })
    });
    // cgroup v2（当前 Docker 部署）和 v1 的固定只读资源文件，不读取环境凭据。
    let cgroup = [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ]
    .into_iter()
    .filter_map(|path| std::fs::read_to_string(path).ok())
    .filter_map(|s| s.trim().parse::<u64>().ok())
    .filter(|v| *v > 0)
    .min();
    physical
        .into_iter()
        .chain(cgroup)
        .min()
        .unwrap_or(2 * 1024 * 1024 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_workers_follow_cpu_and_container_memory_limits() {
        let gib = 1024 * 1024 * 1024;
        assert_eq!(concurrency(0, 8, 6 * gib), 5);
        assert_eq!(concurrency(0, 2, 16 * gib), 2);
        assert_eq!(concurrency(0, 32, 2 * gib), 1);
        assert_eq!(concurrency(3, 8, 6 * gib), 3);
        assert_eq!(concurrency(100, 8, 6 * gib), 5);
        assert_eq!(concurrency(0, 0, 1), 1);
    }

    #[test]
    fn upstream_must_be_a_bare_origin() {
        let config = |upstream: &str| Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            upstream: upstream.into(),
            max_concurrent: 0,
            fetch_timeout_seconds: 20,
        };
        assert!(config("http://minio:9000").validate().is_ok());
        assert!(config("https://s3.example.test").validate().is_ok());
        for bad in [
            "http://admin:secret@minio:9000",
            "http://minio:9000/bucket",
            "http://minio:9000?x=1",
            "ftp://minio",
            "minio:9000",
        ] {
            assert!(config(bad).validate().is_err(), "{bad}");
        }
        let mut c = config("http://minio:9000");
        c.max_concurrent = 129;
        assert!(c.validate().is_err());
        c = config("http://minio:9000");
        c.fetch_timeout_seconds = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn example_configuration_parses() {
        let c: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        c.validate().unwrap();
        assert_eq!(c.upstream, "http://minio:9000");
        assert!(
            toml::from_str::<Config>(
                "listen = \"0.0.0.0:1\"\nupstream = \"http://m:9\"\nsources = []\n"
            )
            .is_err()
        );
    }
}
