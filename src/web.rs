use crate::{
    config::Config,
    error::{Failure, Result},
    source,
    transform::{self, Parameters},
};
use axum::{
    Router,
    body::Body,
    extract::{RawQuery, State},
    http::{HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use std::{sync::Arc, time::Instant};
use tokio::sync::Semaphore;
use tower_http::cors::{AllowOrigin, CorsLayer};

#[derive(Clone)]
struct App {
    config: Arc<Config>,
    client: reqwest::Client,
    slots: Arc<Semaphore>,
}

/// 只记录随机请求号、阶段、错误码、计数和耗时；禁止记录 URI、原图地址、签名或正文。
/// 连接中断使 future 被丢弃时，Drop 仍记录取消终态。
struct Operation {
    id: String,
    start: Instant,
    stage: &'static str,
    finished: bool,
}
impl Operation {
    fn new() -> Self {
        let id = uuid::Uuid::new_v4().to_string();
        tracing::info!(request_id=%id,event="request_start","图片处理开始");
        Self {
            id,
            start: Instant::now(),
            stage: "validate",
            finished: false,
        }
    }
    fn finish(&mut self, result: &Result<Response>) {
        self.finished = true;
        match result {
            Ok(_) => {
                tracing::info!(request_id=%self.id,event="request_end",status="ok",duration_ms=self.start.elapsed().as_millis() as u64,"图片处理完成")
            }
            Err(e) => {
                tracing::warn!(request_id=%self.id,event="request_end",status="failed",stage=self.stage,code=e.1,duration_ms=self.start.elapsed().as_millis() as u64,"图片处理失败")
            }
        }
    }
}
impl Drop for Operation {
    fn drop(&mut self) {
        if !self.finished {
            tracing::info!(request_id=%self.id,event="request_end",status="cancelled",stage=self.stage,duration_ms=self.start.elapsed().as_millis() as u64,"图片处理请求已取消");
        }
    }
}

pub fn app(config: Config) -> std::result::Result<Router, String> {
    config.validate().map_err(str::to_owned)?;
    let origins = config
        .allowed_origins
        .iter()
        .map(|s| s.parse::<HeaderValue>())
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| "浏览器来源配置无效".to_owned())?;
    // 只使用固定回源地址；关闭环境代理、自动重试、重定向及 Referer。
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .referer(false)
        .no_proxy()
        .timeout(config.fetch_timeout())
        .connect_timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|_| "HTTP 客户端初始化失败".to_owned())?;
    let state = App {
        slots: Arc::new(Semaphore::new(config.max_concurrent)),
        client,
        config: Arc::new(config),
    };
    Ok(Router::new()
        .route("/", get(process))
        .route("/healthz", get(|| async { "ok\n" }))
        .with_state(state)
        .layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::list(origins))
                .allow_methods([Method::GET, Method::HEAD])
                .expose_headers(["x-request-id".parse().unwrap()]),
        ))
}
async fn process(State(state): State<App>, RawQuery(query): RawQuery) -> Response {
    let mut op = Operation::new();
    let result = execute(state, query, &mut op).await;
    op.finish(&result);
    let mut response = match result {
        Ok(response) => response,
        Err(error) => error.into_response(),
    };
    for (key, value) in [
        ("cache-control", "no-store"),
        ("referrer-policy", "no-referrer"),
        ("x-content-type-options", "nosniff"),
    ] {
        response.headers_mut().insert(
            axum::http::HeaderName::from_static(key),
            HeaderValue::from_static(value),
        );
    }
    response
        .headers_mut()
        .insert("x-request-id", op.id.parse().unwrap());
    response
}
async fn execute(state: App, query: Option<String>, op: &mut Operation) -> Result<Response> {
    let query = query.ok_or(Failure::invalid("missing_parameters"))?;
    if query.len() > 16384 {
        return Err(Failure::invalid("query_too_long"));
    }
    let params: Parameters =
        serde_urlencoded::from_str(&query).map_err(|_| Failure::invalid("invalid_parameters"))?;
    params.validate()?;
    source::target(&params.url, &state.config.sources)?;
    // 不排队积压；下载和编码共享名额。CPU 工作一旦开始，名额由工作闭包持有，
    // 即使调用者取消也不会提前释放，让后续请求突破并发和内存上限。
    let slot = state
        .slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| Failure(StatusCode::SERVICE_UNAVAILABLE, "busy"))?;
    op.stage = "fetch";
    let bytes = source::fetch(&state.client, &state.config, &params.url).await?;
    tracing::info!(request_id=%op.id,event="source_read",bytes=bytes.len(),"原图读取完成");
    op.stage = "transform";
    let max_pixels = state.config.max_input_pixels;
    let output = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        transform::transform(&bytes, &params, max_pixels)
    })
    .await
    .map_err(|_| Failure(StatusCode::INTERNAL_SERVER_ERROR, "worker_failed"))??;
    tracing::info!(request_id=%op.id,event="image_encoded",bytes=output.bytes.len(),width=output.width,height=output.height,"图片编码完成");
    Ok(Response::builder()
        .header("content-type", output.content_type)
        .body(Body::from(output.bytes))
        .unwrap())
}
