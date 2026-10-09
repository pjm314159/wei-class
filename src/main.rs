//! 微助教签到工具（Rust 重写版）服务入口。

use std::process::ExitCode;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};
use wei_class::config::Config;
use wei_class::server;

#[tokio::main]
async fn main() -> ExitCode {
    let config = Config::from_env();
    let (file_guard, file_error) = init_logging(&config);

    if let Some(err) = file_error {
        tracing::warn!(error = %err, dir = %config.log_dir, "日志目录创建失败，仅输出到控制台");
    }
    tracing::info!(?config, "配置加载完成");

    let result = server::run(config).await;
    // 文件层非阻塞写入：guard 需保活至日志写完，进程退出前显式丢弃以冲刷尾部。
    drop(file_guard);

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = %err, "服务异常退出");
            ExitCode::FAILURE
        }
    }
}

/// 初始化日志：控制台 + 文件（按日滚动、非阻塞写入），级别由 `RUST_LOG` 控制。
///
/// 日志目录创建失败时回退为仅控制台输出（通过第二个返回值告知调用方在
/// 初始化完成后记录告警）。返回的 [`WorkerGuard`] 必须保活至进程结束，
/// 否则日志可能丢失尾部。
fn init_logging(config: &Config) -> (Option<WorkerGuard>, Option<String>) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let appender = rolling::Builder::new()
        .rotation(rolling::Rotation::DAILY)
        .filename_prefix("wei-class")
        .filename_suffix("log")
        .build(&config.log_dir);
    let (file_layer, guard, file_error) = match appender {
        Ok(appender) => {
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let layer = fmt::layer().with_ansi(false).with_writer(writer).boxed();
            (Some(layer), Some(guard), None)
        }
        Err(err) => (None, None, Some(err.to_string())),
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer())
        .with(file_layer)
        .init();
    (guard, file_error)
}
