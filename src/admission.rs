//! 页面图片突发请求的有界准入：等待只存在于当前HTTP请求内，不下载、不启动后台任务。
//! CPU开始后将整个许可交给阻塞工作线程；客户端取消也不能提前释放真实资源预算。
use crate::error::{Failure, Result};
use axum::http::StatusCode;
use std::{sync::Arc, time::Duration};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub(crate) struct Admission {
    workers: Arc<Semaphore>,
    requests: Arc<Semaphore>,
    wait: Duration,
}
#[derive(Debug)]
pub(crate) struct Permit {
    _request: OwnedSemaphorePermit,
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
    pub(crate) async fn acquire(&self) -> Result<Permit> {
        let request = self
            .requests
            .clone()
            .try_acquire_owned()
            .map_err(|_| Failure(StatusCode::SERVICE_UNAVAILABLE, "busy"))?;
        // Tokio信号量按到达顺序唤醒。等待取消/超时会释放队列容量，且从未发出回源请求。
        let worker = tokio::time::timeout(self.wait, self.workers.clone().acquire_owned())
            .await
            .map_err(|_| Failure(StatusCode::SERVICE_UNAVAILABLE, "queue_timeout"))?
            .map_err(|_| Failure(StatusCode::SERVICE_UNAVAILABLE, "busy"))?;
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
        assert_eq!(gate.acquire().await.err().unwrap().1, "busy");
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        assert_eq!(gate.acquire().await.err().unwrap().1, "queue_timeout");
        drop(active);
        assert!(gate.acquire().await.is_ok());
    }
}
