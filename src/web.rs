//! HTTP 入口：MinIO 的公开入口，外加 OSS 同参数的图片处理。
//!
//! 所有请求按原路径、Host、查询串与端到端头转给可信上游；只有 GET 带 `x-oss-process=image/...` 时，
//! 网关取回原图按 OSS 语义加工后返回。HEAD 与 OSS 一致不加工，返回原对象的头。其余方法（含直传 PUT、
//! 分片上传）一律透传，请求与响应正文都是流式转发，不缓冲整个对象。

use crate::{
    admission::Admission,
    cache::Cache,
    config::Config,
    error::{OssError, Result},
    process::{self, Plan, Step},
    proxy,
    transform::{self, MAX_SOURCE_BYTES, Product},
};
use axum::{
    Router,
    body::{Body, HttpBody},
    extract::{Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header},
    response::Response,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

/// 处理请求（排队 + 回源 + 编码）的整体时限：55 秒，在前置 Nginx 60 秒读超时之前返回明确错误。
const PROCESS_DEADLINE: Duration = Duration::from_secs(55);
/// 等待编码名额的时限：45 秒，留出回源与编码的时间。
const QUEUE_WAIT: Duration = Duration::from_secs(45);
/// 除正在编码的任务外，最多再排队 256 个处理请求（一页缩略图的突发量级），再多立即 busy。
const MAX_WAITING: usize = 256;
/// 回源建连时限：5 秒。同网络 MinIO 正常在毫秒级建连。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// 处理器共享状态。
#[derive(Clone)]
struct App {
    /// 可信上游 origin，不带结尾斜线。
    upstream: Arc<str>,
    /// 处理路径回源总时限。
    fetch_timeout: Duration,
    /// 回源客户端：不跟随重定向、不重试、不读环境代理、不加默认头。
    client: reqwest::Client,
    /// 编码名额与队列。
    admission: Arc<Admission>,
    /// 处理结果缓存。
    cache: Arc<Mutex<Cache>>,
}

/// 一次请求的日志上下文。只记录随机请求号、阶段、状态码、错误码、计数和耗时；
/// 禁止记录路径、查询串、Host、签名、请求头或正文。连接中断使 future 被丢弃时，Drop 记录取消终态。
struct Operation {
    /// 随机请求号，也放进响应头 X-Request-Id 与错误正文的 RequestId。
    id: String,
    /// 开始时刻。
    start: Instant,
    /// passthrough（透传）或 process（图片处理）。
    kind: &'static str,
    /// 当前阶段，失败或取消时记入日志。
    stage: &'static str,
    /// 是否已记录终态。
    finished: bool,
}

impl Operation {
    fn new(method: &Method) -> Self {
        let id = uuid::Uuid::new_v4().to_string();
        tracing::info!(request_id=%id,event="request_start",method=%method,"请求开始");
        Self {
            id,
            start: Instant::now(),
            kind: "passthrough",
            stage: "route",
            finished: false,
        }
    }
    fn elapsed(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
    fn succeeded(&mut self, status: StatusCode) {
        self.finished = true;
        tracing::info!(request_id=%self.id,event="request_end",status="ok",kind=self.kind,http_status=status.as_u16(),duration_ms=self.elapsed(),"请求完成");
    }
    fn failed(&mut self, error: &OssError) {
        self.finished = true;
        tracing::warn!(request_id=%self.id,event="request_end",status="failed",kind=self.kind,stage=self.stage,http_status=error.status.as_u16(),code=error.code,ec=error.ec.unwrap_or(""),reason=error.reason,duration_ms=self.elapsed(),"请求失败");
    }
}

impl Drop for Operation {
    fn drop(&mut self) {
        if !self.finished {
            tracing::info!(request_id=%self.id,event="request_end",status="cancelled",kind=self.kind,stage=self.stage,duration_ms=self.elapsed(),"请求已取消");
        }
    }
}

/// 构造网关。配置无效或 HTTP 客户端无法初始化时返回错误说明。
pub fn app(config: Config) -> std::result::Result<Router, String> {
    config.validate().map_err(str::to_owned)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .referer(false)
        .no_proxy()
        // reqwest 默认会补 `Accept: */*`；清空默认头，上游只看到客户端原来的头。
        .default_headers(HeaderMap::new())
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|_| "HTTP 客户端初始化失败".to_owned())?;
    let workers = config.effective_concurrency();
    tracing::info!(
        event = "admission_config",
        workers,
        max_waiting = MAX_WAITING,
        "图片处理并发预算已确定"
    );
    let state = App {
        upstream: config.upstream.trim_end_matches('/').into(),
        fetch_timeout: config.fetch_timeout(),
        client,
        admission: Arc::new(Admission::new(workers, MAX_WAITING, QUEUE_WAIT)),
        cache: Arc::new(Mutex::new(Cache::default())),
    };
    Ok(Router::new().fallback(handle).with_state(state))
}

/// 所有路径、所有方法的入口。
async fn handle(State(app): State<App>, request: Request) -> Response {
    let mut op = Operation::new(request.method());
    let host = host(&request);
    let mut response = match route(&app, request, &host, &mut op).await {
        Ok(response) => {
            op.succeeded(response.status());
            response
        }
        Err(error) => {
            op.failed(&error);
            error.response(&op.id, &host)
        }
    };
    if let Ok(id) = HeaderValue::from_str(&op.id) {
        response.headers_mut().insert("x-request-id", id);
    }
    response
}

/// 请求的 Host；HTTP/2 没有 Host 头时取 URI 的 authority。
fn host(request: &Request) -> String {
    request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or_else(|| request.uri().authority().map(|a| a.to_string()))
        .unwrap_or_default()
}

/// 分流：GET + image 处理串走加工，其余透传。
async fn route(app: &App, request: Request, host: &str, op: &mut Operation) -> Result<Response> {
    if request.method() == Method::GET {
        let query = request.uri().query().unwrap_or("").to_owned();
        if let Some(value) = proxy::process_value(&query) {
            match process::parse(&value)? {
                Plan::Image(steps) => {
                    op.kind = "process";
                    return tokio::time::timeout(
                        PROCESS_DEADLINE,
                        processed(app, request, host, &query, steps, op),
                    )
                    .await
                    .unwrap_or_else(|_| Err(OssError::request_timeout()));
                }
                Plan::NoSuchStyle => {
                    return Err(OssError::new(
                        StatusCode::NOT_FOUND,
                        "NoSuchStyle",
                        "No Such Image Style.",
                        None,
                        "no_such_style",
                    ));
                }
                Plan::Original => {}
            }
        }
    }
    passthrough(app, request, op).await
}

/// 上游地址：可信 origin + 原始路径 + 查询串。`url` 只补编码 URI 里本不该裸露的字符，
/// 已有的百分号编码保持不变，签名参数逐字到达上游。
fn upstream_url(app: &App, path: &str, query: &str) -> Result<reqwest::Url> {
    let text = if query.is_empty() {
        format!("{}{path}", app.upstream)
    } else {
        format!("{}{path}?{query}", app.upstream)
    };
    reqwest::Url::parse(&text).map_err(|_| {
        OssError::new(
            StatusCode::BAD_REQUEST,
            "InvalidURI",
            "Couldn't parse the specified URI.",
            None,
            "invalid_uri",
        )
    })
}

/// 回源失败：超时与连不上分开报，便于运维区分。
fn upstream_error(error: reqwest::Error) -> OssError {
    if error.is_timeout() {
        OssError::upstream_timeout()
    } else {
        OssError::upstream_unreachable()
    }
}

/// 原样透传：请求正文与响应正文都流式转发。
///
/// GET/HEAD 匿名请求在转发前去掉 `x-oss-process`（裸 MinIO 不处理它；HEAD 与 OSS 一致返回原对象的头）；
/// 带签名的请求保持查询串原样，原因见 [`proxy::is_signed`]。其他方法的查询串一律不动。
async fn passthrough(app: &App, request: Request, op: &mut Operation) -> Result<Response> {
    let (parts, body) = request.into_parts();
    let query = parts.uri.query().unwrap_or("");
    let forwarded = if matches!(parts.method, Method::GET | Method::HEAD)
        && !proxy::is_signed(query, &parts.headers)
    {
        proxy::strip_process(query)
    } else {
        query.to_owned()
    };
    let url = upstream_url(app, parts.uri.path(), &forwarded)?;
    let mut builder = app
        .client
        .request(parts.method.clone(), url)
        .headers(with_host(
            &parts,
            proxy::request_headers(&parts.headers, false),
        ));
    // 没有正文的请求不挂正文，免得变成带空 chunked 正文的 GET；有正文时按原 Content-Length 流式转发，
    // 客户端是 chunked 时上游同样收到 chunked。
    if body.size_hint().exact() != Some(0) {
        builder = builder.body(reqwest::Body::wrap_stream(body.into_data_stream()));
    }
    op.stage = "upstream";
    let response = builder.send().await.map_err(upstream_error)?;
    Ok(relay(response))
}

/// HTTP/2 请求没有 Host 头时，用 URI 的 authority 补上，保证 SigV4 签进的 Host 到达上游。
fn with_host(parts: &axum::http::request::Parts, mut headers: HeaderMap) -> HeaderMap {
    if !headers.contains_key(header::HOST)
        && let Some(value) = parts
            .uri
            .authority()
            .and_then(|a| HeaderValue::from_str(a.as_str()).ok())
    {
        headers.insert(header::HOST, value);
    }
    headers
}

/// 把上游应答原样交给客户端：状态码、端到端头、流式正文。
fn relay(response: reqwest::Response) -> Response {
    let status = response.status();
    let headers = proxy::response_headers(response.headers(), false);
    let mut out = Response::new(Body::from_stream(response.bytes_stream()));
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    out
}

/// 图片处理路径。
///
/// 1. 先排队取得编码名额，排队期间不回源。
/// 2. 带客户端的鉴权与条件头向上游 GET 原图（去掉 Range）；客户端没带条件头而缓存里有结果时，
///    附上缓存的 ETag 做 If-None-Match。
/// 3. 上游 304 且是网关自己的条件请求 → 缓存命中；上游 200 且 ETag 与缓存相同 → 命中、不读正文；
///    其余 200 → 读原图（≤20 MB）编码并缓存；上游其他应答（403/404/412、客户端条件请求的 304 等）
///    原样透传。
///
/// 授权每次都由上游判断，被拒的请求绝不会拿到缓存内容。
async fn processed(
    app: &App,
    request: Request,
    host: &str,
    query: &str,
    steps: Vec<Step>,
    op: &mut Operation,
) -> Result<Response> {
    // 处理请求不接受正文（OSS 同样拒绝带正文的处理请求），正文直接丢弃。
    let (parts, _) = request.into_parts();
    let plan = steps
        .iter()
        .map(|s| transform::step_args(s).join(" "))
        .collect::<Vec<_>>()
        .join("|");
    let key = Cache::key(host, parts.uri.path(), &proxy::cache_query(query), &plan);
    let forwarded = if proxy::is_signed(query, &parts.headers) {
        query.to_owned()
    } else {
        proxy::strip_process(query)
    };
    let url = upstream_url(app, parts.uri.path(), &forwarded)?;

    op.stage = "queue";
    let queued = Instant::now();
    let permit = app.admission.acquire().await?;
    tracing::info!(request_id=%op.id,event="request_admitted",wait_ms=queued.elapsed().as_millis() as u64,"图片处理请求取得名额");

    op.stage = "fetch";
    let cached = app.cache.lock().await.get(&key);
    let mut headers = with_host(&parts, proxy::request_headers(&parts.headers, true));
    let client_conditional = headers.contains_key(header::IF_NONE_MATCH)
        || headers.contains_key(header::IF_MODIFIED_SINCE);
    let revalidating = match &cached {
        Some(entry) if !client_conditional => match HeaderValue::from_str(&entry.etag) {
            Ok(etag) => {
                headers.insert(header::IF_NONE_MATCH, etag);
                true
            }
            Err(_) => false,
        },
        _ => false,
    };
    let mut response = app
        .client
        .get(url)
        .headers(headers)
        .timeout(app.fetch_timeout)
        .send()
        .await
        .map_err(upstream_error)?;
    let status = response.status();
    if status == StatusCode::NOT_MODIFIED && revalidating {
        let entry = cached.expect("revalidating implies a cache entry");
        return Ok(finished(
            op,
            response.headers(),
            &entry.meta,
            &entry.product,
            "HIT",
        ));
    }
    if status != StatusCode::OK {
        drop(permit);
        tracing::info!(request_id=%op.id,event="upstream_relayed",http_status=status.as_u16(),"上游应答原样转发");
        return Ok(relay(response));
    }
    let etag = proxy::strong_etag(response.headers());
    // 客户端自己的条件请求没有命中上游，但对象没变：直接用缓存，不读原图正文。
    if let (Some(entry), Some(etag)) = (&cached, &etag)
        && entry.etag == *etag
    {
        return Ok(finished(
            op,
            response.headers(),
            &entry.meta,
            &entry.product,
            "HIT",
        ));
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_SOURCE_BYTES as u64)
    {
        return Err(OssError::source_too_large());
    }
    let upstream_headers = response.headers().clone();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(upstream_error)? {
        if chunk.len() > MAX_SOURCE_BYTES.saturating_sub(bytes.len()) {
            return Err(OssError::source_too_large());
        }
        bytes.extend_from_slice(&chunk);
    }
    tracing::info!(request_id=%op.id,event="source_read",bytes=bytes.len(),"原图读取完成");
    // 空对象、SVG、PDF 等：与 OSS 一样报格式不支持，不进解码器。
    if transform::source_format(&bytes).is_none() {
        return Err(OssError::unsupported_format());
    }

    op.stage = "transform";
    let product = Arc::new(
        tokio::task::spawn_blocking(move || {
            // 名额随阻塞任务一起走到子进程结束；客户端中途断开也不提前释放。
            let _permit = permit;
            transform::run(bytes, &steps)
        })
        .await
        .map_err(|_| OssError::worker_failed())??,
    );
    tracing::info!(request_id=%op.id,event="image_encoded",bytes=product.bytes.len(),width=product.width,height=product.height,"图片处理完成");
    let meta = proxy::object_meta(&upstream_headers);
    let cache_status = match etag {
        Some(etag) => {
            app.cache.lock().await.put(key, etag, meta, product.clone());
            "MISS"
        }
        // 没有强 ETag 就无法判断对象是否变化，不缓存。
        None => "BYPASS",
    };
    Ok(finished(op, &upstream_headers, &[], &product, cache_status))
}

/// 组装处理结果：上游本次应答的端到端头（去掉描述原图正文的头）+ 缓存补齐的对象头 +
/// 结果的 Content-Type / Content-Length + X-Image-Cache。
fn finished(
    op: &Operation,
    current: &HeaderMap,
    fill: &[(HeaderName, HeaderValue)],
    product: &Product,
    cache_status: &'static str,
) -> Response {
    tracing::info!(request_id=%op.id,event="cache_result",cache=cache_status,"处理缓存检查完成");
    let mut headers = proxy::response_headers(current, true);
    for (name, value) in fill {
        if !headers.contains_key(name) {
            headers.insert(name.clone(), value.clone());
        }
    }
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(product.content_type),
    );
    headers.insert(header::CONTENT_LENGTH, product.bytes.len().into());
    headers.insert("x-image-cache", HeaderValue::from_static(cache_status));
    let mut response = Response::new(Body::from(product.bytes.clone()));
    *response.headers_mut() = headers;
    response
}
