//! 处理结果的有界进程内缓存。
//!
//! 只缓存编码结果和描述对象的少量元数据头，不落盘、不存原图或签名。过期与淘汰只在请求里顺带进行，
//! 不启动后台任务。命中前每次仍向上游发条件 GET，由上游按当前授权与 ETag 决定能否复用（见 web.rs）。

use crate::transform::Product;
use axum::http::{HeaderName, HeaderValue};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

/// 缓存总字节上限：256 MiB。处理器容器 6 GiB，扣除 5 个编码名额各约 1 GiB 后留给缓存与运行时。
const MAX_BYTES: usize = 256 * 1024 * 1024;
/// 缓存条目数上限，避免大量极小结果让淘汰扫描变慢。
const MAX_ENTRIES: usize = 512;
/// 单条结果上限：32 MiB（总量的 1/8），更大的结果（如大尺寸 BMP）不缓存，免得一条挤掉整页缩略图。
const MAX_ENTRY_BYTES: usize = MAX_BYTES / 8;
/// 条目寿命：1 小时。上游对象可变时靠 ETag 校验失效；寿命只限制内存长期被冷数据占用。
const TTL: Duration = Duration::from_secs(3600);

/// 一条缓存。
#[derive(Clone)]
pub struct Entry {
    /// 编码结果。
    pub product: Arc<Product>,
    /// 生成结果时上游给出的强 ETag（含引号），下一次用它做 If-None-Match。
    pub etag: String,
    /// 生成结果时上游 200 应答里描述对象的头（Cache-Control、Content-Disposition 等）；
    /// 上游的 304 应答不一定带这些头，命中时用它们补齐。
    pub meta: Vec<(HeaderName, HeaderValue)>,
    /// 最近一次使用时间，LRU 淘汰依据。
    used: Instant,
    /// 写入时间，TTL 依据。
    created: Instant,
}

/// 缓存本体；由调用方加锁使用。
#[derive(Default)]
pub struct Cache {
    /// 键是 [`Cache::key`] 的十六进制摘要。
    entries: HashMap<String, Entry>,
    /// 当前所有条目正文字节数之和。
    bytes: usize,
}

impl Cache {
    /// 缓存键：对象身份（Host + 原始路径）+ 非鉴权查询 + 规范化处理串，取 SHA-256。
    /// 签名参数不进键，同一对象换了签名仍可复用结果；授权每次由上游校验。
    pub fn key(host: &str, path: &str, query: &[(String, String)], plan: &str) -> String {
        let mut text = format!("v3\n{host}\n{path}\n");
        for (k, v) in query {
            text.push_str(&format!("{}={}\n", k.len(), k));
            text.push_str(&format!("{}={}\n", v.len(), v));
        }
        text.push_str(plan);
        format!("{:x}", Sha256::digest(text))
    }

    /// 取出条目并刷新使用时间。
    pub fn get(&mut self, key: &str) -> Option<Entry> {
        self.prune();
        let entry = self.entries.get_mut(key)?;
        entry.used = Instant::now();
        Some(entry.clone())
    }

    /// 丢掉过期条目并重算总字节数。
    fn prune(&mut self) {
        self.entries.retain(|_, e| e.created.elapsed() < TTL);
        self.bytes = self.entries.values().map(|e| e.product.bytes.len()).sum();
    }

    /// 写入一条；超过单条上限的结果直接放弃缓存，空间不够时按最久未用淘汰。
    pub fn put(
        &mut self,
        key: String,
        etag: String,
        meta: Vec<(HeaderName, HeaderValue)>,
        product: Arc<Product>,
    ) {
        self.prune();
        let size = product.bytes.len();
        if size > MAX_ENTRY_BYTES {
            return;
        }
        if let Some(old) = self.entries.remove(&key) {
            self.bytes -= old.product.bytes.len();
        }
        while self.bytes + size > MAX_BYTES || self.entries.len() >= MAX_ENTRIES {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(removed) = self.entries.remove(&oldest) {
                self.bytes -= removed.product.bytes.len();
            }
        }
        self.bytes += size;
        let now = Instant::now();
        self.entries.insert(
            key,
            Entry {
                product,
                etag,
                meta,
                used: now,
                created: now,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn product(size: usize) -> Arc<Product> {
        Arc::new(Product {
            bytes: vec![0; size].into(),
            content_type: "image/webp",
            width: 1,
            height: 1,
        })
    }

    #[test]
    fn key_separates_objects_queries_and_plans() {
        let q = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let base = Cache::key("h", "/b/k", &q(&[]), "plan");
        assert_ne!(base, Cache::key("h2", "/b/k", &q(&[]), "plan"));
        assert_ne!(base, Cache::key("h", "/b/k2", &q(&[]), "plan"));
        assert_ne!(
            base,
            Cache::key("h", "/b/k", &q(&[("versionId", "1")]), "plan")
        );
        assert_ne!(base, Cache::key("h", "/b/k", &q(&[]), "plan2"));
        // 长度前缀让 “a=b” 与 “a” “=b” 这类拼接歧义不撞键。
        assert_ne!(
            Cache::key("h", "/k", &q(&[("a=b", "c")]), "p"),
            Cache::key("h", "/k", &q(&[("a", "b=c")]), "p")
        );
    }

    #[test]
    fn oversized_results_are_not_kept_and_lru_evicts() {
        let mut cache = Cache::default();
        cache.put(
            "big".into(),
            "\"e\"".into(),
            vec![],
            product(MAX_ENTRY_BYTES + 1),
        );
        assert!(cache.get("big").is_none());
        // k0、k1 先写并各隔 2ms，保证使用时间严格早于后写的条目。
        for i in 0..MAX_ENTRIES {
            cache.put(format!("k{i}"), "\"e\"".into(), vec![], product(1));
            if i < 2 {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        // 刷新 k0，之后写入新条目时淘汰的是最久未用的 k1。
        assert!(cache.get("k0").is_some());
        cache.put("new".into(), "\"e\"".into(), vec![], product(1));
        assert!(cache.get("k0").is_some());
        assert!(cache.get("k1").is_none());
        assert!(cache.get("new").is_some());
    }
}
