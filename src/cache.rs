use crate::transform::{Output, Parameters};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

// 有界进程内 LRU，不落盘签名或原图；过期和淘汰只由请求触发，不启动后台任务。
const MAX_BYTES: usize = 256 * 1024 * 1024;
const MAX_ENTRIES: usize = 512;
const TTL: Duration = Duration::from_secs(3600);
#[derive(Clone)]
pub struct Entry {
    pub output: Arc<Output>,
    pub etag: String,
    used: Instant,
    created: Instant,
}
#[derive(Default)]
pub struct Cache {
    entries: HashMap<String, Entry>,
    bytes: usize,
}
impl Cache {
    pub fn key(params: &Parameters) -> String {
        let mut url = url::Url::parse(&params.url).expect("validated URL");
        // 身份、签名及过期时间每次仍交给 MinIO 验证；保留 versionId 等所有资源参数。
        let mut pairs: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(k, _)| {
                ![
                    "X-Amz-Algorithm",
                    "X-Amz-Credential",
                    "X-Amz-Date",
                    "X-Amz-Expires",
                    "X-Amz-SignedHeaders",
                    "X-Amz-Signature",
                    "X-Amz-Security-Token",
                ]
                .contains(&k.as_ref())
            })
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        pairs.sort();
        url.set_query(None);
        url.query_pairs_mut().extend_pairs(pairs);
        format!(
            "{:x}",
            Sha256::digest(format!("{}\n{}", url, params.cache_variant()))
        )
    }
    pub fn get(&mut self, key: &str) -> Option<Entry> {
        self.prune();
        let entry = self.entries.get_mut(key)?;
        entry.used = Instant::now();
        Some(entry.clone())
    }
    fn prune(&mut self) {
        self.entries.retain(|_, e| e.created.elapsed() < TTL);
        self.bytes = self.entries.values().map(|e| e.output.bytes.len()).sum();
    }
    pub fn put(&mut self, key: String, etag: String, output: Arc<Output>) {
        self.prune();
        if output.bytes.len() > MAX_BYTES {
            return;
        }
        if let Some(old) = self.entries.remove(&key) {
            self.bytes -= old.output.bytes.len();
        }
        while self.bytes + output.bytes.len() > MAX_BYTES || self.entries.len() >= MAX_ENTRIES {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.bytes -= self.entries.remove(&oldest).unwrap().output.bytes.len();
        }
        self.bytes += output.bytes.len();
        let now = Instant::now();
        self.entries.insert(
            key,
            Entry {
                output,
                etag,
                used: now,
                created: now,
            },
        );
    }
}
