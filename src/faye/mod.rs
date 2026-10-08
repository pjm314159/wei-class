//! faye（Bayeux 协议）客户端：与微助教 `/faye` 的 WebSocket 通信。
//!
//! 报文模型见 [`messages`]；连接单例与状态机随后续提交落地（PLAN M1）。

pub mod messages;
