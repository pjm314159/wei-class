//! 统一错误类型与结果别名。

use thiserror::Error;

/// 应用统一错误。
///
/// 变体随里程碑逐步扩展（faye、轮询、HTTP 各层各自细分）。
#[derive(Debug, Error)]
pub enum Error {
    /// 配置无效。
    ///
    /// 当前配置解析采用回退语义（见 [`crate::config`]），此变体保留给后续
    /// 引入强制校验的场景（如监听地址解析失败）。
    #[error("无效配置: {0}")]
    Config(String),
}

/// 应用统一结果别名。
pub type Result<T> = std::result::Result<T, Error>;
