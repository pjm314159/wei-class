//! M3 契约测试：静态页、`POST /api/login` 与 `WS /ws` 的全部消息路径。
//!
//! 用真实 axum 服务 + tokio-tungstenite 模拟浏览器，覆盖
//! `Qr / Closed / Congested / SessionExpired / Status`（含 interval 钳制），
//! 并断言活跃连接归零时 faye 收到 `/meta/disconnect`。

mod common;

use std::time::Duration;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{CLOSED_PUSH, CONGESTED_PUSH, COOKIE, QR_PUSH, TIMEOUT, TestResult};

/// 上游 `active_signs` 路径。
const ACTIVE_SIGNS: &str = "/wechat-api/v1/class-attendance/student/active_signs";

/// 启动一套完整测试环境（wiremock 上游 + mock faye + 真实服务）。
macro_rules! setup {
    ($upstream:ident, $faye:ident, $app:ident) => {
        let $upstream = MockServer::start().await;
        let $faye = common::mock_faye().await?;
        let $app = common::spawn_app(common::config(), &$upstream.uri(), &$faye.endpoint()).await?;
    };
}

/// 挂载 `active_signs`：以给定状态码与响应体响应。
async fn mount_active_signs(server: &MockServer, status: u16, body: &str) {
    Mock::given(method("GET"))
        .and(path(ACTIVE_SIGNS))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(server)
        .await;
}

/// 发起一次登录请求。
async fn login(base: &str, openid: &str) -> TestResult<reqwest::Response> {
    Ok(reqwest::Client::new()
        .post(format!("{base}/api/login"))
        .json(&serde_json::json!({ "openid": openid }))
        .send()
        .await?)
}

#[tokio::test]
async fn root_serves_static_page() -> TestResult {
    setup!(upstream, faye, app);
    let _ = &faye;

    let response = reqwest::get(app.base()).await?;
    assert_eq!(response.status(), 200);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        content_type.starts_with("text/html"),
        "content-type = {content_type}"
    );
    let body = response.text().await?;
    assert!(body.contains("<!doctype html"), "body = {body}");
    Ok(())
}

#[tokio::test]
async fn login_issues_two_hour_cookie_on_valid_openid() -> TestResult {
    setup!(upstream, faye, app);
    mount_active_signs(&upstream, 200, "[]").await;

    let response = login(&app.base(), "openid-under-test").await?;
    assert_eq!(response.status(), 200);
    let cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        cookie.contains("openid=openid-under-test"),
        "set-cookie = {cookie}"
    );
    assert!(cookie.contains("Max-Age=7200"), "set-cookie = {cookie}");
    Ok(())
}

#[tokio::test]
async fn login_reports_invalid_token() -> TestResult {
    setup!(upstream, faye, app);
    mount_active_signs(&upstream, 401, "").await;

    let response = login(&app.base(), "openid-under-test").await?;
    assert_eq!(response.status(), 401);
    assert!(response.headers().get("set-cookie").is_none());
    assert_eq!(
        response.text().await?,
        r#"{"ok":false,"reason":"tokenInvalid"}"#
    );
    Ok(())
}

#[tokio::test]
async fn login_reports_network_failure() -> TestResult {
    setup!(upstream, faye, app);
    mount_active_signs(&upstream, 502, "").await;

    let response = login(&app.base(), "openid-under-test").await?;
    assert_eq!(response.status(), 502);
    assert_eq!(response.text().await?, r#"{"ok":false,"reason":"network"}"#);
    Ok(())
}

#[tokio::test]
async fn login_rejects_malformed_openid_before_upstream() -> TestResult {
    setup!(upstream, faye, app);
    mount_active_signs(&upstream, 200, "[]").await;

    let response = login(&app.base(), "bad;value").await?;
    assert_eq!(response.status(), 400);
    assert_eq!(
        response.text().await?,
        r#"{"ok":false,"reason":"invalidOpenid"}"#
    );
    // 非法 openid 不应打到上游。
    let requests = upstream
        .received_requests()
        .await
        .ok_or("上游请求记录不可用")?;
    assert!(
        requests.is_empty(),
        "非法 openid 不应打到上游，实际 {} 条",
        requests.len()
    );
    Ok(())
}

#[tokio::test]
async fn ws_reports_session_expired_without_cookie() -> TestResult {
    setup!(upstream, faye, app);

    let mut ws = common::connect_ws(&app.ws_url(), None).await?;
    let message = common::next_text(&mut ws, TIMEOUT).await?;
    assert_eq!(message.as_deref(), Some(r#"{"type":"sessionExpired"}"#));

    // 服务端随后关闭连接。
    let closed = common::next_text(&mut ws, TIMEOUT).await?;
    assert_eq!(closed, None);
    Ok(())
}

#[tokio::test]
async fn ws_pushes_qr_closed_and_congested() -> TestResult {
    setup!(upstream, faye, app);

    let mut ws = common::connect_ws(&app.ws_url(), Some(COOKIE)).await?;
    // 会话建立即下发 Status（当前间隔 = 服务端下限）。
    let status = common::next_text(&mut ws, TIMEOUT).await?;
    assert_eq!(
        status.as_deref(),
        Some(r#"{"type":"status","intervalMs":1000,"minIntervalMs":1000}"#)
    );

    faye.push.send(String::from(QR_PUSH)).await?;
    faye.push.send(String::from(CLOSED_PUSH)).await?;
    faye.push.send(String::from(CONGESTED_PUSH)).await?;

    let qr = common::next_text(&mut ws, TIMEOUT).await?;
    assert_eq!(
        qr.as_deref(),
        Some(r#"{"type":"qr","signId":200002,"url":"https://example.test/qr/round2"}"#)
    );
    let closed = common::next_text(&mut ws, TIMEOUT).await?;
    assert_eq!(
        closed.as_deref(),
        Some(r#"{"type":"closed","signId":200002}"#)
    );
    let congested = common::next_text(&mut ws, TIMEOUT).await?;
    assert_eq!(
        congested.as_deref(),
        Some(r#"{"type":"congested","signId":200002}"#)
    );
    Ok(())
}

#[tokio::test]
async fn ws_clamps_reported_interval_to_server_minimum() -> TestResult {
    setup!(upstream, faye, app);

    let mut ws = common::connect_ws(&app.ws_url(), Some(COOKIE)).await?;
    let status = common::next_text(&mut ws, TIMEOUT).await?;
    assert_eq!(
        status.as_deref(),
        Some(r#"{"type":"status","intervalMs":1000,"minIntervalMs":1000}"#)
    );

    // 低于下限（0s）→ 钳制到 1000ms。
    common::send_text(&mut ws, r#"{"type":"interval","seconds":0}"#).await?;
    let clamped = common::next_text(&mut ws, TIMEOUT).await?;
    assert_eq!(
        clamped.as_deref(),
        Some(r#"{"type":"status","intervalMs":1000,"minIntervalMs":1000}"#)
    );

    // 高于下限 → 原样生效。
    common::send_text(&mut ws, r#"{"type":"interval","seconds":5}"#).await?;
    let raised = common::next_text(&mut ws, TIMEOUT).await?;
    assert_eq!(
        raised.as_deref(),
        Some(r#"{"type":"status","intervalMs":5000,"minIntervalMs":1000}"#)
    );
    Ok(())
}

#[tokio::test]
async fn ws_sends_keepalive_ping() -> TestResult {
    setup!(upstream, faye, app);

    let mut ws = common::connect_ws(&app.ws_url(), Some(COOKIE)).await?;
    let _status = common::next_text(&mut ws, TIMEOUT).await?;
    common::expect_ping(&mut ws, Duration::from_secs(2)).await?;
    Ok(())
}

#[tokio::test]
async fn ws_ignores_unknown_client_messages() -> TestResult {
    setup!(upstream, faye, app);

    let mut ws = common::connect_ws(&app.ws_url(), Some(COOKIE)).await?;
    let _status = common::next_text(&mut ws, TIMEOUT).await?;

    common::send_text(&mut ws, r#"{"type":"nope"}"#).await?;
    // 无回包：等到一个保活 ping 即证明会话未被异常关闭。
    common::expect_ping(&mut ws, Duration::from_secs(2)).await?;
    Ok(())
}

#[tokio::test]
async fn last_connection_close_shuts_down_faye() -> TestResult {
    setup!(upstream, faye, app);
    let mut faye = faye;

    let mut ws = common::connect_ws(&app.ws_url(), Some(COOKIE)).await?;
    // 活跃连接 0→1：faye 预建连，握手先于任何订阅到达。
    faye.expect_frame("/meta/handshake").await?;

    ws.close(None).await?;
    let disconnect = faye.expect_frame("/meta/disconnect").await?;
    assert!(disconnect.contains("/meta/disconnect"));
    Ok(())
}

#[tokio::test]
async fn faye_stays_up_while_another_connection_lives() -> TestResult {
    setup!(upstream, faye, app);
    let mut faye = faye;

    let mut first = common::connect_ws(&app.ws_url(), Some(COOKIE)).await?;
    faye.expect_frame("/meta/handshake").await?;
    let mut second = common::connect_ws(&app.ws_url(), Some(COOKIE)).await?;

    // 仍有连接存活：不应下发 disconnect。
    first.close(None).await?;
    let early = faye
        .wait_frame("/meta/disconnect", Duration::from_millis(300))
        .await?;
    assert!(early.is_none(), "归零前不应关闭 faye: {early:?}");

    // 最后一条连接断开：归零触发关闭。
    second.close(None).await?;
    let disconnect = faye.expect_frame("/meta/disconnect").await?;
    assert!(disconnect.contains("/meta/disconnect"));
    Ok(())
}
