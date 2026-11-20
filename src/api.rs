//! 微助教 HTTP API 客户端（学生端）。
//!
//! 当前仅封装 [`TeachmateClient::active_signs`]：按 openid 查询活跃签到列表，
//! 用于发现新的二维码签到（`docs/DESIGN.md` §5 `SignPoller` 的数据源）。

use std::time::Duration;

use serde::{Deserialize, Serialize};
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
    /// 是否二维码签到（`1` 是）。
    #[serde(rename = "isQR", default)]
    pub is_qr: i64,
    /// 是否 GPS 签到（`1` 是）；仅 `isQR == 0` 时参与自动提交。
    #[serde(rename = "isGPS", default)]
    pub is_gps: i64,
    /// 签到名称（可用于日志与前端展示）。
    #[serde(default)]
    pub name: String,
}

/// 签到类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignKind {
    /// 二维码签到：走 faye 订阅与扫码链路。
    Qr,
    /// GPS 签到：提交 `courseId/signId`，可选 `lon/lat`。
    Gps,
    /// 普通签到：只提交 `courseId/signId`。
    Normal,
}

impl Sign {
    /// 按 `isQR` / `isGPS` 判定签到类型；`isQR` 优先。
    #[must_use]
    pub fn kind(&self) -> SignKind {
        if self.is_qr != 0 {
            SignKind::Qr
        } else if self.is_gps != 0 {
            SignKind::Gps
        } else {
            SignKind::Normal
        }
    }
}

/// GPS 坐标（GCJ-02）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coordinates {
    /// 经度。
    pub lon: f64,
    /// 纬度。
    pub lat: f64,
}

impl Coordinates {
    /// 校验并构造坐标；非有限数或超出范围时返回 `None`。
    #[must_use]
    pub fn new(lon: f64, lat: f64) -> Option<Self> {
        if lon.is_finite()
            && lat.is_finite()
            && (-180.0..=180.0).contains(&lon)
            && (-90.0..=90.0).contains(&lat)
        {
            Some(Self { lon, lat })
        } else {
            None
        }
    }

    /// 保留 5 位小数并加 ±0.00002 抖动，避免多用户坐标完全一致。
    ///
    /// `seed` 由调用方提供（生产用签到 ID 与时间，测试用固定值），
    /// 通过 xorshift 生成确定性抖动，不引入额外随机依赖。
    #[must_use]
    pub fn normalized(self, seed: u64) -> Self {
        let mut state = seed | 1;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let step = u16::try_from(state % 1_000).unwrap_or(0);
        let offset = f64::from(step) * 0.000_04 / 999.0 - 0.000_02;
        Self {
            lon: round5(self.lon + offset).clamp(-180.0, 180.0),
            lat: round5(self.lat - offset).clamp(-90.0, 90.0),
        }
    }
}

/// 保留 5 位小数。
fn round5(value: f64) -> f64 {
    (value * 100_000.0).round() / 100_000.0
}

/// 签到提交结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInOutcome {
    /// 本次提交成功。
    Signed,
    /// 服务端返回 305：已经签到过。
    AlreadySigned,
    /// 业务错误（`errorCode` 非 0 / 305）。
    Rejected {
        /// 上游错误码（无法解析时为 `None`）。
        code: Option<i64>,
        /// 上游错误消息。
        message: Option<String>,
    },
}

/// 签到提交请求体。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SignInRequest {
    course_id: i64,
    sign_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    lon: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lat: Option<f64>,
}

/// 签到提交响应体。
#[derive(Debug, Deserialize)]
struct SignInResponse {
    #[serde(rename = "errorCode", default)]
    error_code: Option<serde_json::Value>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    msg: Option<String>,
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

    /// 提交普通 / GPS 签到。
    ///
    /// - 普通签到：`location` 传 `None`，body 只含 `courseId/signId`；
    /// - GPS 签到：`location` 传坐标，body 追加 5 位小数的 `lon/lat`。
    ///
    /// # Errors
    /// - [`ApiError::TokenInvalid`]：服务端返回 401/403；
    /// - [`ApiError::Network`]：连接失败、非 2xx 或响应体不是合法 JSON。
    pub async fn sign_in(
        &self,
        openid: &str,
        course_id: i64,
        sign_id: i64,
        location: Option<Coordinates>,
    ) -> Result<SignInOutcome, ApiError> {
        let url = format!(
            "{}/wechat-api/v1/class-attendance/student-sign-in",
            self.base_url
        );
        let (lon, lat) = match location {
            Some(coords) => {
                let normalized = coords.normalized(seed_of(openid, sign_id));
                (Some(normalized.lon), Some(normalized.lat))
            }
            None => (None, None),
        };
        let body = SignInRequest {
            course_id,
            sign_id,
            lon,
            lat,
        };
        let response = self
            .http
            .post(url)
            .header("openId", openid)
            .json(&body)
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

        let text = response
            .text()
            .await
            .map_err(|err| ApiError::Network(err.to_string()))?;
        if text.trim().is_empty() {
            return Ok(SignInOutcome::Signed);
        }
        let parsed: SignInResponse = serde_json::from_str(&text)
            .map_err(|err| ApiError::Network(format!("响应解析失败: {err}")))?;
        let Some(raw_code) = parsed.error_code.as_ref() else {
            return Ok(SignInOutcome::Signed);
        };
        let message = parsed.message.or(parsed.msg);
        let Some(code) = parse_error_code(raw_code) else {
            return Ok(SignInOutcome::Rejected {
                code: None,
                message,
            });
        };
        match code {
            0 => Ok(SignInOutcome::Signed),
            305 => Ok(SignInOutcome::AlreadySigned),
            other => Ok(SignInOutcome::Rejected {
                code: Some(other),
                message,
            }),
        }
    }
}

/// 生成坐标抖动种子：openid 与签到 ID 的组合，同一用户同签到稳定。
fn seed_of(openid: &str, sign_id: i64) -> u64 {
    let mut seed = sign_id.unsigned_abs();
    for byte in openid.bytes() {
        seed = seed.wrapping_mul(31).wrapping_add(u64::from(byte));
    }
    seed
}

/// 解析上游 `errorCode`：兼容数字与字符串。
fn parse_error_code(value: &serde_json::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
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
        assert_eq!(sign.is_gps, 0);
        assert_eq!(sign.kind(), SignKind::Qr);
        assert_eq!(sign.name, "test");
        Ok(())
    }

    #[test]
    fn classifies_sign_kind_with_qr_priority() {
        let mut sign = Sign {
            course_id: 1,
            sign_id: 2,
            is_qr: 0,
            is_gps: 0,
            name: String::new(),
        };
        assert_eq!(sign.kind(), SignKind::Normal);

        sign.is_gps = 1;
        assert_eq!(sign.kind(), SignKind::Gps);

        sign.is_qr = 1;
        assert_eq!(sign.kind(), SignKind::Qr, "isQR 优先于 isGPS");
    }

    #[test]
    fn validates_coordinates() {
        assert!(Coordinates::new(114.440_465, 30.517_877).is_some());
        assert!(Coordinates::new(181.0, 30.0).is_none());
        assert!(Coordinates::new(114.0, -91.0).is_none());
        assert!(Coordinates::new(f64::NAN, 30.0).is_none());
        assert!(Coordinates::new(114.0, f64::INFINITY).is_none());
    }

    #[test]
    fn normalized_coordinates_keep_five_decimals() {
        let coords =
            Coordinates::new(114.440_465, 30.517_877).unwrap_or(Coordinates { lon: 0.0, lat: 0.0 });
        let normalized = coords.normalized(42);
        assert!((normalized.lon - 114.440_465).abs() < 0.000_1);
        assert!((normalized.lat - 30.517_877).abs() < 0.000_1);
        assert!(normalized.lon.is_finite() && normalized.lat.is_finite());
    }

    /// 挂载签到提交响应。
    async fn mount_sign_in(server: &MockServer, body: &str) -> TestResult {
        Mock::given(method("POST"))
            .and(path("/wechat-api/v1/class-attendance/student-sign-in"))
            .and(header("openId", OPENID))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(server)
            .await;
        Ok(())
    }

    #[tokio::test]
    async fn sign_in_sends_gps_coordinates() -> TestResult {
        let server = MockServer::start().await;
        mount_sign_in(&server, r#"{"signRank":33,"studentRank":1}"#).await?;
        let client = client_of(&server)?;
        let coords =
            Coordinates::new(114.440_465, 30.517_877).ok_or_else(|| String::from("坐标应合法"))?;

        let outcome = client
            .sign_in(OPENID, 100_001, 200_003, Some(coords))
            .await?;
        assert_eq!(outcome, SignInOutcome::Signed);

        let requests = server
            .received_requests()
            .await
            .ok_or_else(|| String::from("应收到签到请求"))?;
        let request = requests
            .first()
            .ok_or_else(|| String::from("请求列表为空"))?;
        let body: serde_json::Value = serde_json::from_slice(&request.body)?;
        assert_eq!(body["courseId"], 100_001);
        assert_eq!(body["signId"], 200_003);
        let lon = body["lon"]
            .as_f64()
            .ok_or_else(|| String::from("缺少 lon"))?;
        let lat = body["lat"]
            .as_f64()
            .ok_or_else(|| String::from("缺少 lat"))?;
        assert!((lon - 114.440_465).abs() < 0.000_1, "lon = {lon}");
        assert!((lat - 30.517_877).abs() < 0.000_1, "lat = {lat}");
        Ok(())
    }

    #[tokio::test]
    async fn sign_in_normal_omits_coordinates() -> TestResult {
        let server = MockServer::start().await;
        mount_sign_in(&server, r#"{"signRank":1,"studentRank":1}"#).await?;
        let client = client_of(&server)?;

        let outcome = client.sign_in(OPENID, 100_001, 200_004, None).await?;
        assert_eq!(outcome, SignInOutcome::Signed);

        let requests = server
            .received_requests()
            .await
            .ok_or_else(|| String::from("应收到签到请求"))?;
        let request = requests
            .first()
            .ok_or_else(|| String::from("请求列表为空"))?;
        let body: serde_json::Value = serde_json::from_slice(&request.body)?;
        assert!(body.get("lon").is_none(), "普通签到不应带 lon");
        assert!(body.get("lat").is_none(), "普通签到不应带 lat");
        Ok(())
    }

    #[tokio::test]
    async fn maps_305_to_already_signed() -> TestResult {
        let server = MockServer::start().await;
        mount_sign_in(
            &server,
            r#"{"errorCode":305,"msg":"repeat sign in","msgClient":"你已经签到成功"}"#,
        )
        .await?;
        let client = client_of(&server)?;
        assert_eq!(
            client.sign_in(OPENID, 1, 2, None).await?,
            SignInOutcome::AlreadySigned
        );
        Ok(())
    }

    #[tokio::test]
    async fn maps_business_error_to_rejected() -> TestResult {
        let server = MockServer::start().await;
        mount_sign_in(&server, r#"{"errorCode":4003,"message":"not in range"}"#).await?;
        let client = client_of(&server)?;
        assert_eq!(
            client.sign_in(OPENID, 1, 2, None).await?,
            SignInOutcome::Rejected {
                code: Some(4_003),
                message: Some(String::from("not in range")),
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn sign_in_maps_unauthorized_to_token_invalid() -> TestResult {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let client = client_of(&server)?;
        match client.sign_in(OPENID, 1, 2, None).await {
            Err(ApiError::TokenInvalid) => Ok(()),
            other => Err(format!("期望 TokenInvalid，实际: {other:?}").into()),
        }
    }

    #[tokio::test]
    async fn sign_in_maps_non_json_to_network() -> TestResult {
        let server = MockServer::start().await;
        mount_sign_in(&server, "<html>oops</html>").await?;
        let client = client_of(&server)?;
        match client.sign_in(OPENID, 1, 2, None).await {
            Err(ApiError::Network(msg)) => {
                assert!(msg.contains("响应解析失败"), "msg = {msg}");
                Ok(())
            }
            other => Err(format!("期望 Network，实际: {other:?}").into()),
        }
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
