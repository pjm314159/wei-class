//! M4 端到端契约：Poller → faye `Subscribe` 链路与 `SessionExpired` 传导。
//!
//! 使用真实 axum 服务 + wiremock 上游 + mock faye：断言发现的二维码签到被订阅、
//! 非二维码签到被跳过、同一签到不去重订阅（faye 订阅表去重）、
//! openid 失效时浏览器收到 `sessionExpired`、网络错误不中断会话。

mod common;

use std::time::Duration;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{COOKIE, TIMEOUT, TestResult};

/// 上游 `active_signs` 路径。
const ACTIVE_SIGNS: &str = "/wechat-api/v1/class-attendance/student/active_signs";

/// 二维码签到（虚构课程 100001 / 签到 200002）。
const QR_SIGNS: &str = r#"[{"courseId":100001,"signId":200002,"isGPS":0,"isQR":1,"name":"test"}]"#;

/// 非二维码签到（GPS）：不应进入订阅。
const GPS_SIGNS: &str = r#"[{"courseId":100001,"signId":200002,"isGPS":1,"isQR":0,"name":"test"}]"#;

/// 启动环境：上游固定返回 `body`，轮询下限 `min`。
macro_rules! setup {
    ($upstream:ident, $faye:ident, $app:ident, $body:expr, $min:expr) => {
        let $upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(ACTIVE_SIGNS))
            .respond_with(ResponseTemplate::new(200).set_body_string($body))
            .mount(&$upstream)
            .await;
        let $faye = common::mock_faye().await?;
        let config = wei_class::config::Config {
            min_poll_interval: $min,
            ..common::config()
        };
        let $app = common::spawn_app(config, &$upstream.uri(), &$faye.endpoint()).await?;
    };
}

/// 上游返回指定状态码。
macro_rules! setup_status {
    ($upstream:ident, $faye:ident, $app:ident, $status:expr, $min:expr) => {
        let $upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(ACTIVE_SIGNS))
            .respond_with(ResponseTemplate::new($status))
            .mount(&$upstream)
            .await;
        let $faye = common::mock_faye().await?;
        let config = wei_class::config::Config {
            min_poll_interval: $min,
            ..common::config()
        };
        let $app = common::spawn_app(config, &$upstream.uri(), &$faye.endpoint()).await?;
    };
}

#[tokio::test]
async fn poller_subscribes_discovered_qr_sign_once() -> TestResult {
    setup!(upstream, faye, app, QR_SIGNS, Duration::from_millis(200));
    let mut faye = faye;

    let ws = common::connect_ws(&app.ws_url(), Some(COOKIE)).await?;
    let _ = ws;

    let subscribe = faye.expect_frame("/meta/subscribe").await?;
    assert!(
        subscribe.contains("/attendance/100001/200002/qr"),
        "subscribe = {subscribe}"
    );

    // 去重：后续轮次重复发现同一签到，faye 侧不再发送订阅报文。
    let duplicated = faye
        .wait_frame("/meta/subscribe", Duration::from_millis(600))
        .await?;
    assert!(duplicated.is_none(), "同一签到不应重复订阅: {duplicated:?}");
    Ok(())
}

#[tokio::test]
async fn poller_resumes_after_sign_closed() -> TestResult {
    setup!(upstream, faye, app, QR_SIGNS, Duration::from_millis(200));
    let mut faye = faye;

    let ws = common::connect_ws(&app.ws_url(), Some(COOKIE)).await?;
    let _ = ws;

    // 发现签到 → 订阅。
    let subscribe = faye.expect_frame("/meta/subscribe").await?;
    assert!(subscribe.contains("/attendance/100001/200002/qr"));

    // 上游推送 type:2 → faye 退订并清订阅表 → Poller 恢复轮询 → 再次订阅。
    faye.push.send(String::from(common::CLOSED_PUSH)).await?;
    let unsubscribe = faye.expect_frame("/meta/unsubscribe").await?;
    assert!(unsubscribe.contains("/attendance/100001/200002/qr"));

    let resubscribed = faye
        .wait_frame("/meta/subscribe", Duration::from_secs(3))
        .await?;
    assert!(
        resubscribed.is_some(),
        "退订后应恢复轮询并再次订阅: {resubscribed:?}"
    );
    Ok(())
}

#[tokio::test]
async fn poller_skips_non_qr_signs() -> TestResult {
    setup!(upstream, faye, app, GPS_SIGNS, Duration::from_millis(200));
    let mut faye = faye;

    let ws = common::connect_ws(&app.ws_url(), Some(COOKIE)).await?;
    let _ = ws;

    let subscribe = faye
        .wait_frame("/meta/subscribe", Duration::from_millis(600))
        .await?;
    assert!(subscribe.is_none(), "非二维码签到不应订阅: {subscribe:?}");
    Ok(())
}

#[tokio::test]
async fn invalid_openid_pushes_session_expired() -> TestResult {
    setup_status!(upstream, faye, app, 401, Duration::from_millis(200));

    let mut ws = common::connect_ws(&app.ws_url(), Some(COOKIE)).await?;
    let status = common::next_text(&mut ws, TIMEOUT).await?;
    assert_eq!(
        status.as_deref(),
        Some(r#"{"type":"status","intervalMs":200,"minIntervalMs":200}"#)
    );

    let expired = common::next_text(&mut ws, TIMEOUT).await?;
    assert_eq!(expired.as_deref(), Some(r#"{"type":"sessionExpired"}"#));

    // 服务端随后关闭连接。
    let closed = common::next_text(&mut ws, TIMEOUT).await?;
    assert_eq!(closed, None);
    Ok(())
}

#[tokio::test]
async fn network_error_keeps_session_alive() -> TestResult {
    setup_status!(upstream, faye, app, 502, Duration::from_millis(200));

    let mut ws = common::connect_ws(&app.ws_url(), Some(COOKIE)).await?;
    let _status = common::next_text(&mut ws, TIMEOUT).await?;

    // 网络错误只跳过本轮：不应下发 sessionExpired，会话保持存活。
    let message = common::next_text(&mut ws, Duration::from_millis(600)).await;
    assert!(message.is_err(), "网络错误不应中断会话: {message:?}");
    common::expect_ping(&mut ws, TIMEOUT).await?;
    Ok(())
}
