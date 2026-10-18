//! 网关进程入口：读配置、初始化 JSON 日志、监听并在 SIGTERM/Ctrl-C 时优雅退出。
use image_processor::{config::Config, web};
use tracing_subscriber::prelude::*;

#[tokio::main]
async fn main() {
    // 固定本服务日志级别，不允许外部 RUST_LOG 打开 HTTP 库的签名 URL 调试输出。
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_target(false)
                .with_filter(
                    tracing_subscriber::filter::Targets::new()
                        .with_target("image_processor", tracing::Level::INFO),
                ),
        )
        .init();
    if let Err(message) = run().await {
        tracing::error!(
            event = "startup_failed",
            reason = message,
            "图片处理网关启动失败"
        );
        std::process::exit(1);
    }
}

/// 加载配置并运行到收到退出信号。错误只返回固定说明，不带配置原文。
async fn run() -> Result<(), &'static str> {
    let path = std::env::var("IMAGE_PROCESSOR_CONFIG").unwrap_or_else(|_| "config.toml".into());
    let text = std::fs::read_to_string(path).map_err(|_| "无法读取配置文件")?;
    // 不打印 TOML 原始错误，避免配置值在错误上下文中泄漏。
    let config: Config = toml::from_str(&text).map_err(|_| "配置格式无效")?;
    let address = config.listen;
    let app = web::app(config).map_err(|_| "配置或 HTTP 客户端初始化失败")?;
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|_| "监听地址不可用")?;
    tracing::info!(event="started",listen=%address,"图片处理网关已启动");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await
        .map_err(|_| "HTTP 服务异常退出")?;
    tracing::info!(event = "stopped", "图片处理网关已停止");
    Ok(())
}

/// 等待 SIGTERM（容器停止）或 Ctrl-C。
async fn shutdown() {
    #[cfg(unix)]
    {
        if let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}};
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
