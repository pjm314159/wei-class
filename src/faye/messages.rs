//! faye（Bayeux 协议）报文模型。
//!
//! 覆盖本项目使用的四类 `/meta/*` 请求（handshake / subscribe / connect /
//! disconnect）与对应响应、以及签到频道的 `data` 推送。字段命名遵循 Bayeux
//! 的 camelCase 约定（`clientId` 等）。

use serde::{Deserialize, Serialize};

/// 单条 faye 报文。请求与响应共用此结构，各 channel 只取所需字段，
/// 缺省字段序列化时省略。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    /// 频道名，如 `/meta/handshake`、`/attendance/{course_id}/{sign_id}/qr`。
    pub channel: String,
    /// 报文序号（请求方自定义；响应回显同值）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// 连接标识（handshake 响应下发，其余报文携带）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// 协议版本（仅 handshake 请求）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// 支持的连接类型（仅 handshake 请求）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supported_connection_types: Option<Vec<String>>,
    /// 连接类型（仅 connect 请求）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_type: Option<String>,
    /// 订阅频道名（仅 subscribe / unsubscribe）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription: Option<String>,
    /// 操作是否成功（仅 meta 响应）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub successful: Option<bool>,
    /// 失败原因（仅失败的 meta 响应）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 服务器建议（重连策略等，通常伴随 handshake / connect 响应）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advice: Option<Advice>,
    /// 推送数据（仅业务频道）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// 服务器建议（`advice`）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Advice {
    /// 重连动作：`retry` / `handshake` / `none`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reconnect: Option<String>,
    /// 重连前等待毫秒数。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval: Option<u64>,
    /// 连接保持超时毫秒数（心跳周期依据）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

/// 签到频道推送数据（`data.type == 1` 时含新轮 `qrUrl`）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QrData {
    /// 推送类型：`1` 新轮二维码、`2` 签到关闭、`3` 前方拥挤。
    #[serde(rename = "type")]
    pub kind: i64,
    /// 新一轮二维码 URL（仅 `type == 1`）。
    #[serde(rename = "qrUrl", skip_serializing_if = "Option::is_none")]
    pub qr_url: Option<String>,
}

impl Message {
    /// 构造 handshake 请求。
    #[must_use]
    pub fn handshake(id: &str) -> Self {
        Self {
            channel: String::from("/meta/handshake"),
            id: Some(String::from(id)),
            client_id: None,
            version: Some(String::from("1.0")),
            supported_connection_types: Some(vec![String::from("websocket")]),
            connection_type: None,
            subscription: None,
            successful: None,
            error: None,
            advice: None,
            data: None,
        }
    }

    /// 构造 subscribe 请求。
    #[must_use]
    pub fn subscribe(id: &str, client_id: &str, subscription: &str) -> Self {
        Self {
            channel: String::from("/meta/subscribe"),
            id: Some(String::from(id)),
            client_id: Some(String::from(client_id)),
            version: None,
            supported_connection_types: None,
            connection_type: None,
            subscription: Some(String::from(subscription)),
            successful: None,
            error: None,
            advice: None,
            data: None,
        }
    }

    /// 构造 connect 请求（WS 心跳即重复发送此报文）。
    #[must_use]
    pub fn connect(id: &str, client_id: &str) -> Self {
        Self {
            channel: String::from("/meta/connect"),
            id: Some(String::from(id)),
            client_id: Some(String::from(client_id)),
            version: None,
            supported_connection_types: None,
            connection_type: Some(String::from("websocket")),
            subscription: None,
            successful: None,
            error: None,
            advice: None,
            data: None,
        }
    }

    /// 构造 disconnect 请求。
    #[must_use]
    pub fn disconnect(id: &str, client_id: &str) -> Self {
        Self {
            channel: String::from("/meta/disconnect"),
            id: Some(String::from(id)),
            client_id: Some(String::from(client_id)),
            version: None,
            supported_connection_types: None,
            connection_type: None,
            subscription: None,
            successful: None,
            error: None,
            advice: None,
            data: None,
        }
    }

    /// 将报文包装为单元素 JSON 数组文本（faye 帧格式）。
    #[must_use]
    pub fn to_frame(&self) -> String {
        serde_json::to_string(&[self]).unwrap_or_else(|_| String::from("[]"))
    }

    /// 是否为成功的 meta 响应。
    #[must_use]
    pub fn is_successful(&self) -> bool {
        self.successful == Some(true)
    }
}

/// 从业务推送 `data` 中提取二维码 URL（仅 `type == 1` 且携带 `qrUrl` 时有值）。
#[must_use]
pub fn qr_url_from_data(data: &serde_json::Value) -> Option<String> {
    let parsed: QrData = serde_json::from_value(data.clone()).ok()?;
    if parsed.kind == 1 {
        parsed.qr_url
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HANDSHAKE_RESPONSE: &str = r#"[{"id":"1","channel":"/meta/handshake","successful":true,"clientId":"test-client-id-0123456789abcdef","version":"1.0","supportedConnectionTypes":["websocket","long-polling"],"advice":{"reconnect":"retry","interval":0,"timeout":15000}}]"#;

    const SUBSCRIBE_OK: &str = r#"[{"id":"2","channel":"/meta/subscribe","successful":true,"clientId":"abc","subscription":"/attendance/100001/200002/qr"}]"#;

    const SUBSCRIBE_UNKNOWN_CLIENT: &str = r#"[{"id":"2","channel":"/meta/subscribe","successful":false,"error":"402::x-client-id:Unknown client","clientId":"abc","subscription":"/attendance/100001/200002/qr"}]"#;

    const QR_PUSH: &str = r#"[{"channel":"/attendance/100001/200002/qr","data":{"type":1,"qrUrl":"https://www.teachermate.com.cn/api/v1/qr/attendance/abc"},"clientId":"abc"}]"#;

    const CLOSE_PUSH: &str =
        r#"[{"channel":"/attendance/100001/200002/qr","data":{"type":2},"clientId":"abc"}]"#;

    /// 解析帧文本的首个对象；失败返回空对象（后续断言自然失败）。
    fn first_obj(frame: &str) -> serde_json::Map<String, serde_json::Value> {
        serde_json::from_str::<Vec<serde_json::Value>>(frame)
            .unwrap_or_default()
            .first()
            .and_then(serde_json::Value::as_object)
            .cloned()
            .unwrap_or_default()
    }

    #[test]
    fn handshake_request_serializes_to_bayeux_shape() {
        let msg = first_obj(&Message::handshake("1").to_frame());
        assert_eq!(
            msg.get("channel").and_then(serde_json::Value::as_str),
            Some("/meta/handshake")
        );
        assert_eq!(
            msg.get("version").and_then(serde_json::Value::as_str),
            Some("1.0")
        );
        assert_eq!(
            msg.get("supportedConnectionTypes")
                .and_then(serde_json::Value::as_array)
                .and_then(|types| types.first()),
            Some(&serde_json::Value::String(String::from("websocket")))
        );
        assert!(msg.get("clientId").is_none());
    }

    #[test]
    fn subscribe_request_carries_client_and_subscription() {
        let msg = first_obj(&Message::subscribe("2", "abc", "/attendance/1/2/qr").to_frame());
        assert_eq!(
            msg.get("clientId").and_then(serde_json::Value::as_str),
            Some("abc")
        );
        assert_eq!(
            msg.get("subscription").and_then(serde_json::Value::as_str),
            Some("/attendance/1/2/qr")
        );
    }

    #[test]
    fn connect_request_declares_websocket_type() {
        let msg = first_obj(&Message::connect("3", "abc").to_frame());
        assert_eq!(
            msg.get("connectionType")
                .and_then(serde_json::Value::as_str),
            Some("websocket")
        );
    }

    #[test]
    fn parse_real_handshake_response() {
        let messages: Vec<Message> = serde_json::from_str(HANDSHAKE_RESPONSE).unwrap_or_default();
        assert_eq!(messages.len(), 1);
        let msg = &messages[0];
        assert_eq!(msg.channel, "/meta/handshake");
        assert!(msg.is_successful());
        assert_eq!(
            msg.client_id.as_deref(),
            Some("test-client-id-0123456789abcdef")
        );
        let advice = msg.advice.as_ref().unwrap_or(&Advice {
            reconnect: None,
            interval: None,
            timeout: None,
        });
        assert_eq!(advice.timeout, Some(15_000));
        assert_eq!(advice.reconnect.as_deref(), Some("retry"));
    }

    #[test]
    fn parse_subscribe_responses() {
        let ok: Vec<Message> = serde_json::from_str(SUBSCRIBE_OK).unwrap_or_default();
        assert!(ok[0].is_successful());
        assert_eq!(
            ok[0].subscription.as_deref(),
            Some("/attendance/100001/200002/qr")
        );

        let failed: Vec<Message> =
            serde_json::from_str(SUBSCRIBE_UNKNOWN_CLIENT).unwrap_or_default();
        assert!(!failed[0].is_successful());
        let err_text = failed[0].error.clone().unwrap_or_default();
        assert!(err_text.contains("Unknown client"), "error = {err_text}");
    }

    #[test]
    fn parse_qr_and_close_pushes() {
        let push: Vec<Message> = serde_json::from_str(QR_PUSH).unwrap_or_default();
        assert_eq!(push[0].channel, "/attendance/100001/200002/qr");
        let data = push[0].data.as_ref().unwrap_or(&serde_json::Value::Null);
        assert_eq!(
            qr_url_from_data(data).as_deref(),
            Some("https://www.teachermate.com.cn/api/v1/qr/attendance/abc")
        );

        let close: Vec<Message> = serde_json::from_str(CLOSE_PUSH).unwrap_or_default();
        let close_data = close[0].data.as_ref().unwrap_or(&serde_json::Value::Null);
        assert_eq!(qr_url_from_data(close_data), None);
    }
}
