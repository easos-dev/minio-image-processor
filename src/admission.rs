//! 页面图片突发请求的有界准入：等待只存在于当前 HTTP 请求内，不下载、不启动后台任务。
//! 编码开始后整个许可交给阻塞工作线程；客户端取消也不能提前释放真实资源预算。
use crate::error::{OssError, Result};
use std::{sync::Arc, time::Duration};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// 编码名额与等待队列。
pub(crate) struct Admission {
    /// 同时回源并编码的名额。
    workers: Arc<Semaphore>,
    /// 名额加等待位的总数；拿不到就立即返回 busy。
    requests: Arc<Semaphore>,
    /// 在队列里最多等多久。
    wait: Duration,
}

/// 持有期间占用一个队列位和一个编码名额。
#[derive(Debug)]
pub(crate) struct Permit {
    /// 队列位（含编码名额本身占的位）。
    _request: OwnedSemaphorePermit,
    /// 编码名额。
    _worker: OwnedSemaphorePermit,
}

impl Admission {
    pub(crate) fn new(workers: usize, waiting: usize, wait: Duration) -> Self {
        Self {
            workers: Arc::new(Semaphore::new(workers)),
            requests: Arc::new(Semaphore::new(workers + waiting)),
            wait,
        }
    }

    /// 排队取得编码名额：队列满立即 busy，等满时限返回 queue_timeout。
    pub(crate) async fn acquire(&self) -> Result<Permit> {
        let request = self
            .requests
            .clone()
            .try_acquire_owned()
            .map_err(|_| OssError::busy())?;
        // Tokio 信号量按到达顺序唤醒。等待取消/超时会释放队列容量，且从未发出回源请求。
        let worker = tokio::time::timeout(self.wait, self.workers.clone().acquire_owned())
            .await
            .map_err(|_| OssError::queue_timeout())?
            .map_err(|_| OssError::busy())?;
        Ok(Permit {
            _request: request,
            _worker: worker,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn queue_is_bounded_cancellable_and_times_out_without_leaking_capacity() {
        let gate = Arc::new(Admission::new(1, 1, Duration::from_millis(30)));
        let active = gate.acquire().await.unwrap();
        let g = gate.clone();
        let pending = tokio::spawn(async move { g.acquire().await });
        tokio::task::yield_now().await;
        assert_eq!(gate.acquire().await.err().unwrap().reason, "busy");
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        assert_eq!(gate.acquire().await.err().unwrap().reason, "queue_timeout");
        drop(active);
        assert!(gate.acquire().await.is_ok());
    }
}
