//! `POST /api/login`：校验 openid 并签发 2h cookie（`docs/DESIGN.md` §6）。

use axum::Json;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::AppState;
use super::cookie::{issue, validate};
use crate::api::ApiError;

/// openid 无效或已过期（上游 401/403）。
pub const REASON_TOKEN_INVALID: &str = "tokenInvalid";
/// 上游网络或服务端错误。
pub const REASON_NETWORK: &str = "network";
/// 请求体缺少合法 openid。
pub const REASON_INVALID_OPENID: &str = "invalidOpenid";

/// 登录请求体。
#[derive(Debug, Clone, Deserialize)]
pub struct LoginRequest {
    /// 微助教 openid。
    pub openid: String,
}

/// 登录响应体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LoginResponse {
    /// 是否签发成功。
    pub ok: bool,
    /// 失败原因（成功时省略）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// 校验 openid 一次（`active_signs` 命中即视为有效）并签发 cookie。
pub async fn login(State(state): State<AppState>, Json(body): Json<LoginRequest>) -> Response {
    let Some(openid) = validate(&body.openid) else {
        return failure(StatusCode::BAD_REQUEST, REASON_INVALID_OPENID);
    };
    match state.api().active_signs(&openid).await {
        Ok(_signs) => success(&openid),
        Err(ApiError::TokenInvalid) => failure(StatusCode::UNAUTHORIZED, REASON_TOKEN_INVALID),
        Err(ApiError::Network(err)) => {
            warn!(error = %err, "openid 校验请求失败");
            failure(StatusCode::BAD_GATEWAY, REASON_NETWORK)
        }
    }
}

/// 校验通过：签发 2h cookie。
fn success(openid: &str) -> Response {
    let Some(value) = issue(openid) else {
        return failure(StatusCode::BAD_REQUEST, REASON_INVALID_OPENID);
    };
    let mut response = Json(LoginResponse {
        ok: true,
        reason: None,
    })
    .into_response();
    response.headers_mut().insert(header::SET_COOKIE, value);
    response
}

/// 校验失败：返回状态码与原因，不签发 cookie。
fn failure(status: StatusCode, reason: &str) -> Response {
    let mut response = Json(LoginResponse {
        ok: false,
        reason: Some(String::from(reason)),
    })
    .into_response();
    *response.status_mut() = status;
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_shape_on_success() {
        let response = LoginResponse {
            ok: true,
            reason: None,
        };
        assert_eq!(
            serde_json::to_string(&response).ok().as_deref(),
            Some(r#"{"ok":true}"#)
        );
    }

    #[test]
    fn response_shape_on_failure() {
        let response = LoginResponse {
            ok: false,
            reason: Some(String::from(REASON_TOKEN_INVALID)),
        };
        assert_eq!(
            serde_json::to_string(&response).ok().as_deref(),
            Some(r#"{"ok":false,"reason":"tokenInvalid"}"#)
        );
    }
}
