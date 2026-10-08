//! faye 客户端单例：连接生命周期、订阅表、心跳与事件广播。
//!
//! 对应 `docs/DESIGN.md` §4：
//! - 进程内单例；活跃订阅命令驱动建连，`Shutdown` 或全部命令端关闭时优雅退出；
//! - 断线指数退避重连（全新握手 + 订阅表重放）；`402`/unknown-client 类错误
//!   视为 clientId 失效，跳过退避立即重连；
//! - 心跳即周期发送 `/meta/connect`（首个 tick 立即触发，符合连接周期确认语义）。

use std::collections::HashSet;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tracing::{debug, info, warn};

use super::messages::{Message, qr_url_from_data};

/// faye 客户端配置。
#[derive(Debug, Clone)]
pub struct FayeConfig {
    /// `/faye` WebSocket 端点（如 `wss://www.teachermate.com.cn/faye`）。
    pub endpoint: String,
    /// 心跳间隔（周期发送 `/meta/connect`）。
    pub heartbeat: Duration,
    /// 断线重连退避上限。
    pub backoff_max: Duration,
}

/// 发往客户端单例的命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// 订阅签到频道（幂等：重复订阅不再发送报文）。
    Subscribe {
        /// 课程 ID（频道组成）。
        course_id: i64,
        /// 签到 ID（频道组成）。
        sign_id: i64,
    },
    /// 优雅关闭：尽力发送 disconnect 并结束任务。
    Shutdown,
}

/// 客户端广播给上层的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// 新一轮二维码。
    QrUrl {
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
    /// 前方拥挤提醒。
    Congested {
        /// 签到 ID。
        sign_id: i64,
    },
}

/// 订阅去重键：`(course_id, sign_id)`。
type Subscriptions = HashSet<(i64, i64)>;

/// 客户端内部错误别名。
type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// faye WebSocket 流类型（客户端侧，可能经 TLS）。
type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// 启动客户端任务，返回命令发送端与事件接收端。
#[must_use]
pub fn spawn(config: FayeConfig) -> (mpsc::Sender<Command>, broadcast::Receiver<Event>) {
    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let (event_tx, event_rx) = broadcast::channel(64);
    tokio::spawn(run(config, cmd_rx, event_tx));
    (cmd_tx, event_rx)
}

/// 客户端主循环：空闲等待 → 建连 → 订阅重放 → 活跃循环 → 断线善后。
async fn run(
    config: FayeConfig,
    mut cmd_rx: mpsc::Receiver<Command>,
    event_tx: broadcast::Sender<Event>,
) {
    let mut subscriptions = Subscriptions::new();
    let mut counter: u64 = 0;
    let mut backoff = Duration::from_secs(1).min(config.backoff_max);

    'outer: loop {
        // 空闲等待：订阅表为空时阻塞至首条命令。
        if subscriptions.is_empty() && !wait_for_command(&mut cmd_rx, &mut subscriptions).await {
            break 'outer;
        }

        // 建连（指数退避重试，可被关闭指令打断）。
        let Some((mut ws, client_id)) = establish(
            &config,
            &mut cmd_rx,
            &mut counter,
            &mut backoff,
            &mut subscriptions,
        )
        .await
        else {
            break 'outer;
        };
        backoff = Duration::from_secs(1).min(config.backoff_max);
        info!(client_id = %client_id, "faye 连接就绪");

        // 订阅表重放（建连与重连后统一走此路径）。
        if let Replay::Failed = replay(&mut ws, &client_id, &subscriptions, &mut counter).await {
            if !sleep_or_stop(&mut cmd_rx, backoff).await {
                break 'outer;
            }
            backoff = (backoff * 2).min(config.backoff_max);
            continue 'outer;
        }

        // 活跃循环：心跳 / 命令 / 推送三路复用。
        match active(
            ws,
            client_id,
            &config,
            &mut cmd_rx,
            &mut counter,
            &event_tx,
            &mut subscriptions,
        )
        .await
        {
            ActiveExit::Stop => break 'outer,
            ActiveExit::Retry { immediate } => {
                if subscriptions.is_empty() {
                    continue 'outer;
                }
                if !immediate && !sleep_or_stop(&mut cmd_rx, backoff).await {
                    break 'outer;
                }
                backoff = (backoff * 2).min(config.backoff_max);
            }
        }
    }
}

/// 活跃循环的退出方式。
enum ActiveExit {
    /// 收到关闭指令，任务终止。
    Stop,
    /// 连接断开，需要重连；`immediate` 表示跳过退避（clientId 失效场景）。
    Retry { immediate: bool },
}

/// 活跃循环：处理心跳、命令与服务器推送。
async fn active(
    mut ws: WsStream,
    client_id: String,
    config: &FayeConfig,
    cmd_rx: &mut mpsc::Receiver<Command>,
    counter: &mut u64,
    event_tx: &broadcast::Sender<Event>,
    subscriptions: &mut Subscriptions,
) -> ActiveExit {
    let mut heartbeat = tokio::time::interval(config.heartbeat);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                let frame = Message::connect(&next_id(counter), &client_id).to_frame();
                if let Err(err) = ws.send(WsMessage::text(frame)).await {
                    warn!(error = %err, "faye 心跳发送失败，转入重连");
                    return ActiveExit::Retry { immediate: false };
                }
            }
            command = cmd_rx.recv() => match command {
                None | Some(Command::Shutdown) => {
                    shutdown_graceful(&mut ws, &client_id, counter).await;
                    return ActiveExit::Stop;
                }
                Some(Command::Subscribe { course_id, sign_id }) => {
                    if subscriptions.insert((course_id, sign_id)) {
                        let frame = Message::subscribe(
                            &next_id(counter),
                            &client_id,
                            &channel(course_id, sign_id),
                        )
                        .to_frame();
                        if let Err(err) = ws.send(WsMessage::text(frame)).await {
                            warn!(error = %err, "faye 订阅发送失败，转入重连");
                            return ActiveExit::Retry { immediate: false };
                        }
                    }
                }
            },
            incoming = ws.next() => match incoming {
                Some(Ok(WsMessage::Text(text))) => match handle_frame(&text, event_tx) {
                    FrameOutcome::Ok => {}
                    FrameOutcome::UnknownClient => {
                        warn!("faye clientId 失效，立即重新握手");
                        return ActiveExit::Retry { immediate: true };
                    }
                },
                Some(Ok(WsMessage::Ping(payload))) => {
                    if let Err(err) = ws.send(WsMessage::Pong(payload)).await {
                        warn!(error = %err, "faye Pong 发送失败，转入重连");
                        return ActiveExit::Retry { immediate: false };
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(err)) => {
                    warn!(error = %err, "faye 连接读错误，转入重连");
                    return ActiveExit::Retry { immediate: false };
                }
                None => {
                    warn!("faye 连接被对端关闭，转入重连");
                    return ActiveExit::Retry { immediate: false };
                }
            },
        }
    }
}

/// 空闲阶段等待首条命令；返回 `false` 表示任务应终止。
async fn wait_for_command(
    cmd_rx: &mut mpsc::Receiver<Command>,
    subscriptions: &mut Subscriptions,
) -> bool {
    match cmd_rx.recv().await {
        None | Some(Command::Shutdown) => false,
        Some(Command::Subscribe { course_id, sign_id }) => {
            subscriptions.insert((course_id, sign_id));
            true
        }
    }
}

/// 建连循环：重试直至成功；期间到达的订阅命令进入队列（订阅表）。
/// 返回 `None` 表示收到关闭指令，任务应终止。
async fn establish(
    config: &FayeConfig,
    cmd_rx: &mut mpsc::Receiver<Command>,
    counter: &mut u64,
    backoff: &mut Duration,
    subscriptions: &mut Subscriptions,
) -> Option<(WsStream, String)> {
    loop {
        tokio::select! {
            result = try_connect(config, counter) => match result {
                Ok(session) => return Some(session),
                Err(err) => warn!(error = %err, ?backoff, "faye 建连失败，稍后重试"),
            },
            command = cmd_rx.recv() => match command {
                None | Some(Command::Shutdown) => return None,
                Some(Command::Subscribe { course_id, sign_id }) => {
                    subscriptions.insert((course_id, sign_id));
                }
            },
        }
        tokio::time::sleep(*backoff).await;
        *backoff = (*backoff * 2).min(config.backoff_max);
    }
}

/// 订阅表重放结果。
enum Replay {
    /// 全部发送成功。
    Done,
    /// 发送失败（连接已不可用）。
    Failed,
}

/// 将订阅表逐条重放到当前连接。
async fn replay(
    ws: &mut WsStream,
    client_id: &str,
    subscriptions: &Subscriptions,
    counter: &mut u64,
) -> Replay {
    for &(course_id, sign_id) in subscriptions {
        let frame = Message::subscribe(&next_id(counter), client_id, &channel(course_id, sign_id))
            .to_frame();
        if let Err(err) = ws.send(WsMessage::text(frame)).await {
            warn!(error = %err, "faye 订阅重放发送失败，转入重连");
            return Replay::Failed;
        }
    }
    Replay::Done
}

/// 睡眠退避时长；期间收到关闭指令时返回 `false` 表示终止。
async fn sleep_or_stop(cmd_rx: &mut mpsc::Receiver<Command>, backoff: Duration) -> bool {
    tokio::select! {
        () = tokio::time::sleep(backoff) => true,
        command = cmd_rx.recv() => !matches!(command, None | Some(Command::Shutdown)),
    }
}

/// 建连并完成 handshake，返回 WS 流与服务器下发的 clientId。
async fn try_connect(
    config: &FayeConfig,
    counter: &mut u64,
) -> Result<(WsStream, String), BoxError> {
    let (mut ws, _) = connect_async(&config.endpoint).await?;
    ws.send(WsMessage::text(
        Message::handshake(&next_id(counter)).to_frame(),
    ))
    .await?;
    loop {
        let Some(msg) = ws.next().await else {
            return Err(String::from("连接在握手阶段被对端关闭").into());
        };
        match msg? {
            WsMessage::Text(text) => {
                for m in parse_frame(&text) {
                    if m.channel != "/meta/handshake" {
                        continue;
                    }
                    if m.is_successful()
                        && let Some(id) = m.client_id
                    {
                        return Ok((ws, id));
                    }
                    return Err(format!("faye 握手被拒绝: {}", m.error.unwrap_or_default()).into());
                }
            }
            WsMessage::Ping(payload) => ws.send(WsMessage::Pong(payload)).await?,
            _ => {}
        }
    }
}

/// 帧处理结果。
enum FrameOutcome {
    /// 正常。
    Ok,
    /// clientId 失效，需要重新握手。
    UnknownClient,
}

/// 解析并处理一帧文本：meta 响应检查错误码，业务推送转换为事件。
fn handle_frame(text: &str, event_tx: &broadcast::Sender<Event>) -> FrameOutcome {
    for msg in parse_frame(text) {
        if msg.channel.starts_with("/meta/") {
            if let Some(err_text) = msg.error.as_deref()
                && is_unknown_client(err_text)
            {
                return FrameOutcome::UnknownClient;
            }
            continue;
        }
        let Some((_, sign_id)) = parse_channel(&msg.channel) else {
            continue;
        };
        let Some(data) = msg.data.as_ref() else {
            continue;
        };
        let kind = data
            .get("type")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or_default();
        match kind {
            1 => {
                if let Some(url) = qr_url_from_data(data) {
                    broadcast_event(event_tx, Event::QrUrl { sign_id, url });
                }
            }
            2 => broadcast_event(event_tx, Event::Closed { sign_id }),
            3 => broadcast_event(event_tx, Event::Congested { sign_id }),
            _ => debug!(kind, channel = %msg.channel, "未知的签到推送类型"),
        }
    }
    FrameOutcome::Ok
}

/// 解析帧文本；解析失败返回空列表（faye 帧永远是 JSON 数组）。
fn parse_frame(text: &str) -> Vec<Message> {
    serde_json::from_str(text).unwrap_or_default()
}

/// 优雅关闭：尽力发送 disconnect 并关闭流。
async fn shutdown_graceful(ws: &mut WsStream, client_id: &str, counter: &mut u64) {
    let frame = Message::disconnect(&next_id(counter), client_id).to_frame();
    if let Err(err) = ws.send(WsMessage::text(frame)).await {
        debug!(error = %err, "faye disconnect 发送失败（忽略）");
    }
    let _ = ws.close(None).await;
}

/// 广播事件；无接收者时丢弃并记录调试日志。
fn broadcast_event(event_tx: &broadcast::Sender<Event>, event: Event) {
    if let Err(err) = event_tx.send(event) {
        debug!(error = %err, "faye 事件无接收者，已丢弃");
    }
}

/// 构造签到频道名。
#[must_use]
fn channel(course_id: i64, sign_id: i64) -> String {
    format!("/attendance/{course_id}/{sign_id}/qr")
}

/// 从频道名解析 `(course_id, sign_id)`。
#[must_use]
fn parse_channel(channel: &str) -> Option<(i64, i64)> {
    let parts: Vec<&str> = channel.split('/').collect();
    if parts.len() == 5 && parts[1] == "attendance" && parts[4] == "qr" {
        return Some((parts[2].parse().ok()?, parts[3].parse().ok()?));
    }
    None
}

/// 生成下一报文序号。
fn next_id(counter: &mut u64) -> String {
    *counter += 1;
    counter.to_string()
}

/// 判断错误文本是否为 clientId 失效（Bayeux 402 错误码）。
#[must_use]
fn is_unknown_client(error: &str) -> bool {
    error.starts_with("402") || error.contains("Unknown client")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::timeout;
    use tokio_tungstenite::accept_async;

    use super::*;

    const HANDSHAKE_OK: &str = r#"[{"id":"1","channel":"/meta/handshake","successful":true,"clientId":"mock-client","version":"1.0","supportedConnectionTypes":["websocket"],"advice":{"reconnect":"retry","interval":0,"timeout":15000}}]"#;
    const SUBSCRIBE_OK: &str = r#"[{"id":"2","channel":"/meta/subscribe","successful":true,"clientId":"mock-client","subscription":"/attendance/1447611/4113862/qr"}]"#;
    const SUBSCRIBE_REJECTED: &str = r#"[{"id":"2","channel":"/meta/subscribe","successful":false,"error":"402::x-client-id:Unknown client","clientId":"mock-client","subscription":"/attendance/1447611/4113862/qr"}]"#;
    const QR_PUSH: &str = r#"[{"channel":"/attendance/1447611/4113862/qr","data":{"type":1,"qrUrl":"https://example.test/qr/round2"},"clientId":"mock-client"}]"#;

    type ServerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// mock 服务器侧的明文 WebSocket 流。
    type ServerStream = WebSocketStream<tokio::net::TcpStream>;

    /// 读取下一帧文本；跳过非文本帧，超时或流关闭返回 `None`。
    async fn read_text(ws: &mut ServerStream) -> Option<String> {
        for _ in 0..10 {
            match timeout(Duration::from_secs(2), ws.next()).await {
                Ok(Some(Ok(WsMessage::Text(text)))) => return Some(text.to_string()),
                Ok(Some(Ok(_))) => {}
                _ => return None,
            }
        }
        None
    }

    fn client_config(addr: std::net::SocketAddr) -> FayeConfig {
        FayeConfig {
            endpoint: format!("ws://{addr}"),
            heartbeat: Duration::from_secs(60),
            backoff_max: Duration::from_millis(20),
        }
    }

    #[tokio::test]
    async fn subscribes_and_receives_qr_push() -> ServerResult {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server: tokio::task::JoinHandle<ServerResult> = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let mut ws = accept_async(stream).await?;
            let handshake = read_text(&mut ws).await.ok_or("no handshake")?;
            assert!(handshake.contains("/meta/handshake"));
            ws.send(WsMessage::text(HANDSHAKE_OK)).await?;
            let subscribe = read_text(&mut ws).await.ok_or("no subscribe")?;
            assert!(subscribe.contains("/meta/subscribe"));
            ws.send(WsMessage::text(SUBSCRIBE_OK)).await?;
            ws.send(WsMessage::text(QR_PUSH)).await?;
            Ok(())
        });

        let (cmd_tx, mut event_rx) = spawn(client_config(addr));
        cmd_tx
            .send(Command::Subscribe {
                course_id: 1_447_611,
                sign_id: 4_113_862,
            })
            .await?;
        let event = timeout(Duration::from_secs(5), event_rx.recv()).await??;
        assert_eq!(
            event,
            Event::QrUrl {
                sign_id: 4_113_862,
                url: String::from("https://example.test/qr/round2"),
            }
        );
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn unknown_client_triggers_fresh_handshake() -> ServerResult {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server: tokio::task::JoinHandle<ServerResult> = tokio::spawn(async move {
            // 第一条连接：订阅被 402 拒绝。
            let (stream, _) = listener.accept().await?;
            let mut ws = accept_async(stream).await?;
            read_text(&mut ws).await.ok_or("no handshake #1")?;
            ws.send(WsMessage::text(HANDSHAKE_OK)).await?;
            read_text(&mut ws).await.ok_or("no subscribe #1")?;
            ws.send(WsMessage::text(SUBSCRIBE_REJECTED)).await?;
            // 客户端应立即重连并完成全新握手 + 订阅重放。
            let (stream, _) = listener.accept().await?;
            let mut ws = accept_async(stream).await?;
            read_text(&mut ws).await.ok_or("no handshake #2")?;
            ws.send(WsMessage::text(HANDSHAKE_OK)).await?;
            let subscribe = read_text(&mut ws).await.ok_or("no subscribe #2")?;
            assert!(subscribe.contains("/meta/subscribe"));
            Ok(())
        });

        let (cmd_tx, _event_rx) = spawn(client_config(addr));
        cmd_tx
            .send(Command::Subscribe {
                course_id: 1_447_611,
                sign_id: 4_113_862,
            })
            .await?;
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_sends_disconnect() -> ServerResult {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server: tokio::task::JoinHandle<ServerResult> = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let mut ws = accept_async(stream).await?;
            read_text(&mut ws).await.ok_or("no handshake")?;
            ws.send(WsMessage::text(HANDSHAKE_OK)).await?;
            read_text(&mut ws).await.ok_or("no subscribe")?;
            ws.send(WsMessage::text(SUBSCRIBE_OK)).await?;
            // 心跳首 tick 的 connect 帧可能先于 disconnect 到达，跳过之。
            let mut disconnect = None;
            for _ in 0..5 {
                let Some(text) = read_text(&mut ws).await else { break };
                if text.contains("/meta/disconnect") {
                    disconnect = Some(text);
                    break;
                }
            }
            let disconnect = disconnect.ok_or("no disconnect")?;
            assert!(disconnect.contains("/meta/disconnect"));
            Ok(())
        });

        let (cmd_tx, _event_rx) = spawn(client_config(addr));
        cmd_tx
            .send(Command::Subscribe {
                course_id: 1_447_611,
                sign_id: 4_113_862,
            })
            .await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        cmd_tx.send(Command::Shutdown).await?;
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn dropped_connection_replays_subscriptions() -> ServerResult {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server: tokio::task::JoinHandle<ServerResult> = tokio::spawn(async move {
            // 第一条连接：订阅后直接关闭（不响应订阅）。
            let (stream, _) = listener.accept().await?;
            let mut ws = accept_async(stream).await?;
            read_text(&mut ws).await.ok_or("no handshake #1")?;
            ws.send(WsMessage::text(HANDSHAKE_OK)).await?;
            read_text(&mut ws).await.ok_or("no subscribe #1")?;
            drop(ws);
            // 第二条连接：应收到订阅重放。
            let (stream, _) = listener.accept().await?;
            let mut ws = accept_async(stream).await?;
            read_text(&mut ws).await.ok_or("no handshake #2")?;
            ws.send(WsMessage::text(HANDSHAKE_OK)).await?;
            let subscribe = read_text(&mut ws).await.ok_or("no subscribe replay")?;
            assert!(subscribe.contains("/meta/subscribe"));
            Ok(())
        });

        let (cmd_tx, _event_rx) = spawn(client_config(addr));
        cmd_tx
            .send(Command::Subscribe {
                course_id: 1_447_611,
                sign_id: 4_113_862,
            })
            .await?;
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn heartbeat_sends_connect_periodically() -> ServerResult {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let server: tokio::task::JoinHandle<ServerResult> = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let mut ws = accept_async(stream).await?;
            read_text(&mut ws).await.ok_or("no handshake")?;
            ws.send(WsMessage::text(HANDSHAKE_OK)).await?;
            read_text(&mut ws).await.ok_or("no subscribe")?;
            ws.send(WsMessage::text(SUBSCRIBE_OK)).await?;
            // 统计后续帧中的 connect 心跳。
            let mut connects = 0;
            for _ in 0..6 {
                let Some(text) = read_text(&mut ws).await else {
                    break;
                };
                if text.contains("/meta/connect") {
                    connects += 1;
                }
            }
            assert!(connects >= 2, "心跳帧不足: {connects}");
            Ok(())
        });

        let (cmd_tx, _event_rx) = spawn(FayeConfig {
            endpoint: format!("ws://{addr}"),
            heartbeat: Duration::from_millis(30),
            backoff_max: Duration::from_millis(20),
        });
        cmd_tx
            .send(Command::Subscribe {
                course_id: 1_447_611,
                sign_id: 4_113_862,
            })
            .await?;
        server.await??;
        Ok(())
    }
}
