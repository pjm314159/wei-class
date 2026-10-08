//! `WS /ws`：会话生命周期、事件转发与保活（`docs/DESIGN.md` §3 / §6）。
//!
//! - 会话建立即取得 faye 租约（活跃计数 0→1 触发预建连，归零触发关闭）；
//! - 每连接一条 `watch` 间隔通道：客户端上报 → 服务端按下限钳制 → `Status` 确认；
//! - 每 `WS_PING_MS` 下发一个 ping 帧，防反代/浏览器空闲断开。

use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::Response;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::interval;
use tracing::{debug, info, warn};

use super::AppState;
use super::cookie::parse as parse_openid;
use crate::faye::client::Event;
use crate::poller::{Deps, Poller, clamp};
use crate::protocol::{ClientMessage, ServerMessage};

/// WS 升级入口：缺少有效 cookie 时立即下发 `SessionExpired` 并关闭。
pub async fn handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Response {
    let openid = parse_openid(&headers);
    ws.on_upgrade(move |socket| async move {
        match openid {
            Some(openid) => session(socket, state, openid).await,
            None => expired(socket).await,
        }
    })
}

/// 一条浏览器会话：持有 faye 租约、间隔通道、Poller 与保活定时器，直到连接断开。
///
/// - 会话即 spawn 一个 [`Poller`]：以 openid 轮询活跃签到并驱动 faye 订阅；
/// - Poller 的下行出口（`SessionExpired`）与浏览器推送共用同一条连接。
async fn session(socket: WebSocket, state: AppState, openid: String) {
    let lease = state.faye().acquire();
    let mut events = lease.events();
    let min = state.config().min_poll_interval;

    let (interval_tx, interval_rx) = watch::channel(min);
    let (out_tx, mut out_rx) = mpsc::channel(8);
    let (stop_tx, stop_rx) = oneshot::channel();
    let poller = Poller::new(
        openid,
        state.api().clone(),
        Deps {
            min,
            interval_rx,
            faye_tx: lease.command().clone(),
            out_tx,
            stop_rx,
        },
    );
    let poller_task = tokio::spawn(poller.run());

    let (mut ws_tx, mut ws_rx) = socket.split();

    if send(&mut ws_tx, &ServerMessage::status(millis(min), millis(min)))
        .await
        .is_err()
    {
        return;
    }

    let mut ping = interval(state.config().ws_ping);
    // 丢弃立即就绪的首个 tick：首个 ping 在一个周期后发出。
    ping.tick().await;

    loop {
        tokio::select! {
            _ = ping.tick() => {
                if ws_tx.send(Message::Ping(Bytes::new())).await.is_err() {
                    break;
                }
            }
            inbound = ws_rx.next() => match inbound {
                Some(Ok(Message::Text(text))) => {
                    let outcome = report_interval(&text, min, &interval_tx, &mut ws_tx).await;
                    if outcome.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(err)) => {
                    debug!(error = %err, "浏览器 WS 读错误，结束会话");
                    break;
                }
                Some(Ok(_)) => {}
            },
            message = out_rx.recv() => match message {
                Some(message) => {
                    let expired = matches!(message, ServerMessage::SessionExpired);
                    if send(&mut ws_tx, &message).await.is_err() {
                        break;
                    }
                    // 前端收到后清 cookie 并断开：服务端同步关闭。
                    if expired {
                        break;
                    }
                }
                None => break,
            },
            event = events.recv() => match event {
                Ok(event) => {
                    if send(&mut ws_tx, &server_message(event)).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(skipped)) => warn!(skipped, "faye 事件积压已跳过"),
                Err(RecvError::Closed) => break,
            },
        }
    }
    // 丢弃发送端即向 Poller 发出结束通知；abort 兜底避免等待当前间隔走完。
    drop(stop_tx);
    poller_task.abort();
    info!("浏览器会话结束");
}

/// 无有效 cookie：下发 `SessionExpired` 后关闭（前端清 cookie 并回登录态）。
async fn expired(mut socket: WebSocket) {
    if let Some(json) = ServerMessage::SessionExpired.encode()
        && socket.send(Message::from(json)).await.is_err()
    {
        debug!("SessionExpired 下发失败，直接关闭");
    }
    let _ = socket.close().await;
}

/// 处理上行 `interval`：写入 `watch` 通道并回 `Status` 确认钳制后的值。
///
/// 钳制规则与 Poller 的读取点共用 [`clamp`]，回显值即下轮实际生效值。
async fn report_interval(
    text: &str,
    min: Duration,
    interval_tx: &watch::Sender<Duration>,
    ws_tx: &mut SplitSink<WebSocket, Message>,
) -> Result<(), axum::Error> {
    let Some(ClientMessage::Interval { seconds }) = ClientMessage::decode(text) else {
        debug!(message = %text, "忽略无法识别的上行消息");
        return Ok(());
    };
    let requested = Duration::from_secs(seconds);
    interval_tx.send_replace(requested);
    let effective = clamp(requested, min);
    send(
        ws_tx,
        &ServerMessage::status(millis(effective), millis(min)),
    )
    .await
}

/// 发送一条下行消息；连接不可用时返回错误由调用方结束会话。
async fn send(
    ws_tx: &mut SplitSink<WebSocket, Message>,
    message: &ServerMessage,
) -> Result<(), axum::Error> {
    let Some(json) = message.encode() else {
        return Ok(());
    };
    ws_tx.send(Message::from(json)).await
}

/// faye 事件 → 下行消息。
fn server_message(event: Event) -> ServerMessage {
    match event {
        Event::QrUrl { sign_id, url } => ServerMessage::Qr { sign_id, url },
        Event::Closed { sign_id } => ServerMessage::Closed { sign_id },
        Event::Congested { sign_id } => ServerMessage::Congested { sign_id },
    }
}

/// 毫秒数（超出 `u64` 时取上限，仅用于 `Status` 展示）。
fn millis(value: Duration) -> u64 {
    u64::try_from(value.as_millis()).unwrap_or(u64::MAX)
}
