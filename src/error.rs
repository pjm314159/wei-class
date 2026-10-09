//! 统一错误类型与结果别名。

use thiserror::Error;

/// 应用统一错误。
///
/// 变体随里程碑逐步扩展（faye、轮询、HTTP 各层各自细分）。
#[derive(Debug, Error)]
pub enum Error {
    /// 配置无效（如监听地址无法解析）。
    #[error("无效配置: {0}")]
    Config(String),
    /// 外部依赖初始化失败（如 HTTP 客户端构造）。
    #[error("初始化失败: {0}")]
    Init(String),
    /// I/O 错误（监听绑定、服务运行）。
    #[error("I/O 错误: {0}")]
    Io(#[from] std::io::Error),
}

/// 应用统一结果别名。
pub type Result<T> = std::result::Result<T, Error>;
