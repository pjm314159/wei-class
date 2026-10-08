//! 微助教签到工具（Rust 重写版）服务入口。

use std::process::ExitCode;

use wei_class::config::Config;
use wei_class::server;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env();
    tracing::info!(?config, "配置加载完成");

    match server::run(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = %err, "服务异常退出");
            ExitCode::FAILURE
        }
    }
}
