//! M4 端到端契约：Poller → faye `Subscribe` 链路与 `SessionExpired` 传导。
//!
//! 使用真实 axum 服务 + wiremock 上游 + mock faye：断言二维码签到被订阅、
//! 普通 / GPS 签到自动提交、同一签到不重复提交、openid 失效时浏览器收到
//! `sessionExpired`、网络错误不中断会话。

mod common;

use std::time::Duration;

use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

use common::{COOKIE, TIMEOUT, TestResult};

/// 上游 `active_signs` 路径。
const ACTIVE_SIGNS: &str = "/wechat-api/v1/class-attendance/student/active_signs";

/// 上游 `student-sign-in` 路径。
const SIGN_IN: &str = "/wechat-api/v1/class-attendance/student-sign-in";

/// 二维码签到（虚构课程 100001 / 签到 200002）。
const QR_SIGNS: &str = r#"[{"courseId":100001,"signId":200002,"isGPS":0,"isQR":1,"name":"test"}]"#;

/// 非二维码签到（GPS）：不进入订阅，由 HTTP 提交。
const GPS_SIGNS: &str = r#"[{"courseId":100001,"signId":200002,"isGPS":1,"isQR":0,"name":"test"}]"#;

/// 普通签到：不进入订阅，由 HTTP 提交且不带坐标。
const NORMAL_SIGNS: &str =
    r#"[{"courseId":100001,"signId":200004,"isGPS":0,"isQR":0,"name":"normal"}]"#;

/// 挂载签到提交响应。
async fn mount_sign_in(server: &MockServer, body: &str) {
    Mock::given(method("POST"))
        .and(path(SIGN_IN))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(server)
        .await;
}

/// 读取某个路径收到的请求体。
async fn bodies_for(server: &MockServer, target: &str) -> Vec<Value> {
    let requests = server.received_requests().await.unwrap_or_default();
    requests
        .iter()
        .filter(|request| request.url.path() == target)
        .filter_map(|request| serde_json::from_slice(&request.body).ok())
        .collect()
}

/// 统计某个路径收到的请求数。
async fn count_requests(server: &MockServer, target: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|request| request.url.path() == target)
        .count()
}

/// GPS 提交响应：带 `lon` 成功，不带则返回业务错误（模拟有范围限制）。
struct GpsSignIn;

impl Respond for GpsSignIn {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or_default();
        if body.get("lon").is_some() && body.get("lat").is_some() {
            ResponseTemplate::new(200).set_body_string(r#"{"signRank":1,"studentRank":1}"#)
        } else {
            ResponseTemplate::new(200)
                .set_body_string(r#"{"errorCode":4003,"message":"location required"}"#)
        }
    }
}

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
async fn poller_submits_gps_sign_and_dedups() -> TestResult {
    setup!(upstream, faye, app, GPS_SIGNS, Duration::from_millis(300));
    let mut faye = faye;
    mount_sign_in(&upstream, r#"{"signRank":1,"studentRank":1}"#).await;

    let mut ws = common::connect_ws(&app.ws_url(), Some(COOKIE)).await?;
    let _status = common::next_text(&mut ws, TIMEOUT).await?;
    common::send_text(
        &mut ws,
        r#"{"type":"location","lon":114.440465,"lat":30.517877}"#,
    )
    .await?;

    let message = common::next_text(&mut ws, TIMEOUT).await?;
    let message = message.unwrap_or_default();
    assert!(
        message.contains(r#""type":"signResult""#) && message.contains(r#""status":"ok""#),
        "message = {message}"
    );

    // 至少再跑一轮：签到仍在 active_signs，但本地已去重。
    tokio::time::sleep(Duration::from_millis(700)).await;
    let bodies = bodies_for(&upstream, SIGN_IN).await;
    assert_eq!(bodies.len(), 1, "同一签到不应重复提交");
    let lon = bodies[0]["lon"].as_f64().unwrap_or_default();
    let lat = bodies[0]["lat"].as_f64().unwrap_or_default();
    assert!((lon - 114.440_465).abs() < 0.000_1, "lon = {lon}");
    assert!((lat - 30.517_877).abs() < 0.000_1, "lat = {lat}");

    // 非二维码不订阅 faye。
    let subscribe = faye
        .wait_frame("/meta/subscribe", Duration::from_millis(300))
        .await?;
    assert!(subscribe.is_none(), "GPS 签到不应订阅 faye");
    Ok(())
}

#[tokio::test]
async fn poller_submits_normal_sign_without_coordinates() -> TestResult {
    setup!(
        upstream,
        faye,
        app,
        NORMAL_SIGNS,
        Duration::from_millis(300)
    );
    let _ = faye;
    mount_sign_in(&upstream, r#"{"signRank":1,"studentRank":1}"#).await;

    let mut ws = common::connect_ws(&app.ws_url(), Some(COOKIE)).await?;
    let _status = common::next_text(&mut ws, TIMEOUT).await?;

    let message = common::next_text(&mut ws, TIMEOUT).await?;
    let message = message.unwrap_or_default();
    assert!(
        message.contains(r#""mode":"normal""#),
        "message = {message}"
    );
    assert!(message.contains(r#""status":"ok""#), "message = {message}");

    let bodies = bodies_for(&upstream, SIGN_IN).await;
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].get("lon").is_none(), "普通签到不应带 lon");
    assert!(bodies[0].get("lat").is_none(), "普通签到不应带 lat");
    Ok(())
}

#[tokio::test]
async fn poller_requests_location_after_gps_rejection_then_retries() -> TestResult {
    setup!(upstream, faye, app, GPS_SIGNS, Duration::from_millis(300));
    let _ = faye;
    Mock::given(method("POST"))
        .and(path(SIGN_IN))
        .respond_with(GpsSignIn)
        .mount(&upstream)
        .await;

    let mut ws = common::connect_ws(&app.ws_url(), Some(COOKIE)).await?;
    let _status = common::next_text(&mut ws, TIMEOUT).await?;

    let notice = common::next_text(&mut ws, TIMEOUT).await?;
    let notice = notice.unwrap_or_default();
    assert!(
        notice.contains(r#""type":"locationRequired""#),
        "notice = {notice}"
    );

    common::send_text(
        &mut ws,
        r#"{"type":"location","lon":114.440465,"lat":30.517877}"#,
    )
    .await?;
    let result = common::next_text(&mut ws, TIMEOUT).await?;
    let result = result.unwrap_or_default();
    assert!(
        result.contains(r#""type":"signResult""#) && result.contains(r#""status":"ok""#),
        "result = {result}"
    );

    let bodies = bodies_for(&upstream, SIGN_IN).await;
    assert_eq!(bodies.len(), 2, "应先无坐标失败、再带坐标重试");
    assert!(bodies[0].get("lon").is_none());
    assert!(bodies[1].get("lon").is_some());
    Ok(())
}

#[tokio::test]
async fn poller_reports_already_signed_on_305() -> TestResult {
    setup!(upstream, faye, app, GPS_SIGNS, Duration::from_millis(300));
    let _ = faye;
    mount_sign_in(
        &upstream,
        r#"{"errorCode":305,"msg":"repeat sign in","msgClient":"你已经签到成功"}"#,
    )
    .await;

    let mut ws = common::connect_ws(&app.ws_url(), Some(COOKIE)).await?;
    let _status = common::next_text(&mut ws, TIMEOUT).await?;
    common::send_text(
        &mut ws,
        r#"{"type":"location","lon":114.440465,"lat":30.517877}"#,
    )
    .await?;

    let message = common::next_text(&mut ws, TIMEOUT).await?;
    let message = message.unwrap_or_default();
    assert!(
        message.contains(r#""status":"already""#),
        "message = {message}"
    );
    Ok(())
}

#[tokio::test]
async fn poller_stops_and_resumes_polling() -> TestResult {
    setup!(upstream, faye, app, GPS_SIGNS, Duration::from_millis(200));
    let _ = faye;
    mount_sign_in(&upstream, r#"{"signRank":1,"studentRank":1}"#).await;

    let mut ws = common::connect_ws(&app.ws_url(), Some(COOKIE)).await?;
    let _status = common::next_text(&mut ws, TIMEOUT).await?;

    // 停止轮询：服务端回显确认。
    common::send_text(&mut ws, r#"{"type":"polling","enabled":false}"#).await?;
    let ack = common::next_text(&mut ws, TIMEOUT).await?;
    let ack = ack.unwrap_or_default();
    assert!(
        ack.contains(r#""type":"polling""#) && ack.contains(r#""enabled":false"#),
        "ack = {ack}"
    );

    // 等在途的一轮结束，再取基线；之后不应再有 active_signs 请求。
    tokio::time::sleep(Duration::from_millis(300)).await;
    let base = count_requests(&upstream, ACTIVE_SIGNS).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(
        count_requests(&upstream, ACTIVE_SIGNS).await,
        base,
        "停止轮询后不应继续查询 active_signs"
    );

    // 恢复轮询：立即查询并完成自动提交。
    common::send_text(&mut ws, r#"{"type":"polling","enabled":true}"#).await?;
    let ack = common::next_text(&mut ws, TIMEOUT).await?;
    let ack = ack.unwrap_or_default();
    assert!(ack.contains(r#""enabled":true"#), "ack = {ack}");

    let result = common::next_text(&mut ws, TIMEOUT).await?;
    let result = result.unwrap_or_default();
    assert!(
        result.contains(r#""type":"signResult""#),
        "result = {result}"
    );
    assert!(
        count_requests(&upstream, ACTIVE_SIGNS).await > base,
        "恢复后应重新查询"
    );
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
