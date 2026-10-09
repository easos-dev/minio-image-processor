use serde::Deserialize;
use std::{net::SocketAddr, time::Duration};
use url::Url;

/// 来源 origin 是签名绑定的公开地址；upstream 是运维指定的固定回源地址。
/// 请求者不能指定新的回源主机，重定向也不跟随。内网 HTTP 仅用于受控容器网络。
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub origin: String,
    pub upstream: String,
    pub path_prefix: String,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub allowed_origins: Vec<String>,
    pub sources: Vec<Source>,
    #[serde(default = "input_bytes")]
    pub max_input_bytes: usize,
    #[serde(default = "input_pixels")]
    pub max_input_pixels: u64,
    #[serde(default = "workers")]
    pub max_concurrent: usize,
    #[serde(default = "timeout")]
    pub fetch_timeout_seconds: u64,
}
fn input_bytes() -> usize {
    100 * 1024 * 1024
}
// 与 OSS 静态像素/动图累计像素上限一致；解码进程的内存和时间限制仍独立生效。
fn input_pixels() -> u64 {
    250_000_000
}
fn workers() -> usize {
    0
}
fn timeout() -> u64 {
    20
}

pub fn origin(value: &str) -> std::result::Result<Url, &'static str> {
    let url = Url::parse(value).map_err(|_| "地址格式无效")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.origin().ascii_serialization() != value
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("地址必须是无凭据、路径、查询及片段的 HTTP(S) origin");
    }
    Ok(url)
}
impl Config {
    pub fn validate(&self) -> std::result::Result<(), &'static str> {
        if self.sources.is_empty()
            || self.sources.len() > 16
            || self.allowed_origins.is_empty()
            || self.allowed_origins.len() > 32
        {
            return Err("须明确配置来源存储及浏览器来源");
        }
        if self.max_concurrent > 128
            || !(1..=60).contains(&self.fetch_timeout_seconds)
            || !(1..=100 * 1024 * 1024).contains(&self.max_input_bytes)
            || !(1..=250_000_000).contains(&self.max_input_pixels)
        {
            return Err("资源限制超出范围");
        }
        for value in &self.allowed_origins {
            origin(value)?;
        }
        let mut seen = std::collections::HashSet::new();
        for source in &self.sources {
            origin(&source.origin)?;
            origin(&source.upstream)?;
            if !seen.insert(&source.origin) {
                return Err("来源 origin 不能重复");
            }
            let path = &source.path_prefix;
            if !path.starts_with('/')
                || !path.ends_with('/')
                || path.contains(['%', '\\', '?', '#'])
                || path.split('/').any(|p| p == "." || p == "..")
            {
                return Err("来源目录前缀须为明确的绝对目录");
            }
        }
        Ok(())
    }
    /// 0表示按可用CPU自动选择；显式值也受内存上限约束，避免容器OOM影响整页。
    pub fn effective_concurrency(&self) -> usize {
        let cpus = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1);
        concurrency(self.max_concurrent, cpus, memory_budget())
    }
    pub fn fetch_timeout(&self) -> Duration {
        Duration::from_secs(self.fetch_timeout_seconds)
    }
}

/// 每个worker最多768MiB地址空间，加原图缓冲/tmpfs约1GiB；另留512MiB给缓存和运行时。
fn concurrency(configured: usize, cpus: usize, memory: u64) -> usize {
    let memory_slots = memory.saturating_sub(512 * 1024 * 1024) / (1024 * 1024 * 1024);
    let requested = if configured == 0 { cpus } else { configured };
    requested.clamp(1, 128).min(memory_slots.max(1) as usize)
}
fn memory_budget() -> u64 {
    let physical = std::fs::read_to_string("/proc/meminfo").ok().and_then(|s| {
        s.lines().find_map(|line| {
            line.strip_prefix("MemTotal:")
                .and_then(|v| v.split_whitespace().next())
                .and_then(|v| v.parse::<u64>().ok())
                .and_then(|kb| kb.checked_mul(1024))
        })
    });
    // cgroup v2（当前Docker部署）和v1的固定只读资源文件，绝不读取环境凭据。
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
}
