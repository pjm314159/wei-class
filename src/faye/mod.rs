//! faye（Bayeux 协议）客户端：与微助教 `/faye` 的 WebSocket 通信。
//!
//! 报文模型见 [`messages`]；连接单例与状态机见 [`client`]，活跃连接驱动的
//! 启停见 [`pool`]。

pub mod client;
pub mod messages;
pub mod pool;
