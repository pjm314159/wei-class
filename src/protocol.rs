//! 浏览器 ↔ 服务端 WebSocket 消息契约（`docs/DESIGN.md` §6）。
//!
//! 契约一经合入即锁定：前端只是协议消费者，全部下行消息路径由 Rust 集成测试
//! 覆盖（`tests/`），前端据此无协议错误空间。
//!
//! 上行（浏览器 → 服务端）：
//!
//! ```json
//! {"type":"interval","seconds":5}
//! ```
//!
//! 下行（服务端 → 浏览器）：
//!
//! ```json
//! {"type":"qr","signId":200002,"url":"https://example.test/qr/round2"}
//! {"type":"closed","signId":200002}
//! {"type":"congested","signId":200002}
//! {"type":"sessionExpired"}
//! {"type":"status","intervalMs":5000,"minIntervalMs":1000}
//! ```

use serde::{Deserialize, Serialize};

/// 浏览器上行消息。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ClientMessage {
    /// 指定轮询间隔（秒）：服务端按下限钳制后回 [`ServerMessage::Status`] 确认。
    Interval {
        /// 期望的轮询间隔秒数（低于服务端下限的值将被钳制）。
        seconds: u64,
    },
}

/// 服务端下行消息。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ServerMessage {
    /// 新一轮二维码。
    Qr {
        /// 签到 ID。
        sign_id: i64,
        /// 二维码 URL。
        url: String,
    },
    /// 签到已关闭。
    Closed {
        /// 签到 ID。
        sign_id: i64,
    },
    /// 前方拥挤。
    Congested {
        /// 签到 ID。
        sign_id: i64,
    },
    /// openid 已失效：前端清 cookie 并回到登录态。
    SessionExpired,
    /// 当前生效的轮询间隔与服务端下限：连接建立与间隔变更时下发。
    Status {
        /// 当前生效的轮询间隔（毫秒，已按下限钳制）。
        interval_ms: u64,
        /// 服务端下限（毫秒，来自 `MIN_POLL_INTERVAL_MS`）。
        min_interval_ms: u64,
    },
}

impl ClientMessage {
    /// 解析上行文本帧；未知类型或格式非法时返回 `None`。
    #[must_use]
    pub fn decode(text: &str) -> Option<Self> {
        serde_json::from_str(text).ok()
    }
}

impl ServerMessage {
    /// 序列化为下行文本帧内容。
    #[must_use]
    pub fn encode(&self) -> Option<String> {
        serde_json::to_string(self).ok()
    }

    /// 构造 `Status`：间隔与下限均以毫秒表示。
    #[must_use]
    pub fn status(interval_ms: u64, min_interval_ms: u64) -> Self {
        Self::Status {
            interval_ms,
            min_interval_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(message: &ServerMessage, key: &str) -> Option<String> {
        let json = message.encode()?;
        let value: serde_json::Value = serde_json::from_str(&json).ok()?;
        Some(value.get(key)?.to_string())
    }

    #[test]
    fn server_messages_use_camel_case_tags() {
        let qr = ServerMessage::Qr {
            sign_id: 200_002,
            url: String::from("https://example.test/qr/round2"),
        };
        assert_eq!(
            qr.encode().as_deref(),
            Some(r#"{"type":"qr","signId":200002,"url":"https://example.test/qr/round2"}"#)
        );
        assert_eq!(
            ServerMessage::Closed { sign_id: 200_002 }
                .encode()
                .as_deref(),
            Some(r#"{"type":"closed","signId":200002}"#)
        );
        assert_eq!(
            ServerMessage::Congested { sign_id: 200_002 }
                .encode()
                .as_deref(),
            Some(r#"{"type":"congested","signId":200002}"#)
        );
        assert_eq!(
            ServerMessage::SessionExpired.encode().as_deref(),
            Some(r#"{"type":"sessionExpired"}"#)
        );
        assert_eq!(
            ServerMessage::status(5_000, 1_000).encode().as_deref(),
            Some(r#"{"type":"status","intervalMs":5000,"minIntervalMs":1000}"#)
        );
    }

    #[test]
    fn status_carries_millisecond_fields() {
        let status = ServerMessage::status(1_500, 1_000);
        assert_eq!(field(&status, "intervalMs").as_deref(), Some("1500"));
        assert_eq!(field(&status, "minIntervalMs").as_deref(), Some("1000"));
    }

    #[test]
    fn interval_message_parses_seconds() {
        assert_eq!(
            ClientMessage::decode(r#"{"type":"interval","seconds":5}"#),
            Some(ClientMessage::Interval { seconds: 5 })
        );
    }

    #[test]
    fn unknown_messages_are_rejected() {
        assert_eq!(ClientMessage::decode(r#"{"type":"nope"}"#), None);
        assert_eq!(ClientMessage::decode("not json"), None);
    }

    #[test]
    fn server_messages_round_trip() {
        for message in [
            ServerMessage::Qr {
                sign_id: 1,
                url: String::from("u"),
            },
            ServerMessage::Closed { sign_id: 2 },
            ServerMessage::Congested { sign_id: 3 },
            ServerMessage::SessionExpired,
            ServerMessage::status(2_000, 1_000),
        ] {
            let json = message.encode().unwrap_or_default();
            assert_eq!(
                serde_json::from_str::<ServerMessage>(&json).ok(),
                Some(message)
            );
        }
    }
}
