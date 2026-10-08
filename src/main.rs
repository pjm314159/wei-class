//! 微助教签到工具（Rust 重写版）服务入口。
//!
//! 当前为 M0 骨架：日志初始化与配置加载；服务端与 faye 客户端随后续里程碑合入。

use std::process::ExitCode;

use wei_class::config::Config;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env();
    tracing::info!(?config, "配置加载完成");

    ExitCode::SUCCESS
}
