//! 微助教 HTTP API 客户端（学生端）。
//!
//! 当前仅封装 [`TeachmateClient::active_signs`]：按 openid 查询活跃签到列表，
//! 用于发现新的二维码签到（`docs/DESIGN.md` §5 `SignPoller` 的数据源）。

use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

/// 默认 API 基地址（学生端 wechat-api 网关）。
pub const DEFAULT_BASE_URL: &str = "https://v18.teachermate.cn";

/// 请求使用的浏览器 UA（服务端对缺失 UA 的请求可能拒绝）。
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36 Edg/122.0.0.0";

/// 单次请求超时。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// API 客户端错误。
#[derive(Debug, Error)]
pub enum ApiError {
    /// openid 失效或未授权（HTTP 401/403）。
    #[error("openid 无效或已过期")]
    TokenInvalid,
    /// 网络、服务端或响应格式错误。
    #[error("网络错误: {0}")]
    Network(String),
}

/// 活跃签到条目（响应为 JSON 数组，未使用字段由 serde 忽略）。
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Sign {
    /// 课程 ID（签到频道组成）。
    pub course_id: i64,
    /// 签到 ID（签到频道组成）。
    pub sign_id: i64,
    /// 是否二维码签到（`1` 是）；GPS 签到不入订阅。
    #[serde(rename = "isQR")]
    pub is_qr: i64,
    /// 签到名称（可用于日志展示）。
    #[serde(default)]
    pub name: String,
}

/// 微助教 API 客户端。
#[derive(Debug, Clone)]
pub struct TeachmateClient {
    http: reqwest::Client,
    base_url: String,
}

impl TeachmateClient {
    /// 使用默认基地址创建客户端。
    ///
    /// # Errors
    /// 底层 HTTP 客户端初始化失败（TLS 后端初始化异常）时返回 [`ApiError::Network`]。
    pub fn new() -> Result<Self, ApiError> {
        Self::with_base_url(DEFAULT_BASE_URL)
    }

    /// 使用指定基地址创建客户端（测试用）。
    ///
    /// # Errors
    /// 同 [`TeachmateClient::new`]。
    pub fn with_base_url(base_url: &str) -> Result<Self, ApiError> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .map_err(|err| ApiError::Network(err.to_string()))?;
        Ok(Self {
            http,
            base_url: String::from(base_url),
        })
    }

    /// 查询 openid 下的活跃签到列表。
    ///
    /// # Errors
    /// - [`ApiError::TokenInvalid`]：服务端返回 401/403；
    /// - [`ApiError::Network`]：连接失败、非 2xx 或响应体不是预期的 JSON 数组。
    pub async fn active_signs(&self, openid: &str) -> Result<Vec<Sign>, ApiError> {
        let url = format!(
            "{}/wechat-api/v1/class-attendance/student/active_signs",
            self.base_url
        );
        let response = self
            .http
            .get(url)
            .header("Openid", openid)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|err| ApiError::Network(err.to_string()))?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ApiError::TokenInvalid);
        }
        if !status.is_success() {
            return Err(ApiError::Network(format!("HTTP {status}")));
        }

        response
            .json::<Vec<Sign>>()
            .await
            .map_err(|err| ApiError::Network(format!("响应解析失败: {err}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const OPENID: &str = "openid-under-test";
    /// 实测响应样例（仅取关键字段，其余字段应被 serde 忽略）。
    const SIGNS_BODY: &str = r#"[
        {"courseId": 100001, "signId": 200002, "isGPS": 0, "isQR": 1,
         "name": "test", "code": "QM060", "startYear": 2025, "term": "秋",
         "cover": "https://app.teachermate.com.cn/covers/other2.png"}
    ]"#;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn client_of(server: &MockServer) -> Result<TeachmateClient, ApiError> {
        TeachmateClient::with_base_url(&server.uri())
    }

    #[tokio::test]
    async fn parses_active_signs_with_openid_header() -> TestResult {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/wechat-api/v1/class-attendance/student/active_signs"))
            .and(header("Openid", OPENID))
            .respond_with(ResponseTemplate::new(200).set_body_string(SIGNS_BODY))
            .mount(&server)
            .await;

        let client = client_of(&server)?;
        let signs = client.active_signs(OPENID).await?;

        assert_eq!(signs.len(), 1);
        let sign = &signs[0];
        assert_eq!(sign.course_id, 100_001);
        assert_eq!(sign.sign_id, 200_002);
        assert_eq!(sign.is_qr, 1);
        assert_eq!(sign.name, "test");
        Ok(())
    }

    #[tokio::test]
    async fn maps_unauthorized_to_token_invalid() -> TestResult {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let client = client_of(&server)?;
        match client.active_signs(OPENID).await {
            Err(ApiError::TokenInvalid) => Ok(()),
            other => Err(format!("期望 TokenInvalid，实际: {other:?}").into()),
        }
    }

    #[tokio::test]
    async fn maps_server_error_to_network() -> TestResult {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&server)
            .await;

        let client = client_of(&server)?;
        match client.active_signs(OPENID).await {
            Err(ApiError::Network(msg)) => {
                assert!(msg.contains("502"), "msg = {msg}");
                Ok(())
            }
            other => Err(format!("期望 Network，实际: {other:?}").into()),
        }
    }

    #[tokio::test]
    async fn maps_malformed_body_to_network() -> TestResult {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"msg":"unexpected"}"#))
            .mount(&server)
            .await;

        let client = client_of(&server)?;
        match client.active_signs(OPENID).await {
            Err(ApiError::Network(msg)) => {
                assert!(msg.contains("响应解析失败"), "msg = {msg}");
                Ok(())
            }
            other => Err(format!("期望 Network，实际: {other:?}").into()),
        }
    }

    #[tokio::test]
    async fn maps_connection_refused_to_network() -> TestResult {
        // 绑定后立刻释放端口，保证连接必然失败。
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            listener.local_addr()?.port()
        };
        let client = TeachmateClient::with_base_url(&format!("http://127.0.0.1:{port}"))?;
        match client.active_signs(OPENID).await {
            Err(ApiError::Network(_)) => Ok(()),
            other => Err(format!("期望 Network，实际: {other:?}").into()),
        }
    }
}
