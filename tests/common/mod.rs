//! 集成测试公共装置：真实 axum 服务 + mock faye 服务器 + 浏览器侧 WS 客户端。
//!
//! 契约测试（`tests/server_contract.rs`）在此之上驱动 `/api/login` 与 `/ws`
//! 的全部消息路径。

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::ClientRequestBuilder;
use tokio_tungstenite::tungstenite::http::Uri;
use tokio_tungstenite::{WebSocketStream, accept_async, connect_async};

use wei_class::api::TeachmateClient;
use wei_class::config::Config;
use wei_class::faye::client::FayeConfig;
use wei_class::faye::pool::FayePool;
use wei_class::server::{AppState, router};

/// 测试统一错误类型。
pub type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// 浏览器侧 WS 流类型。
pub type BrowserWs = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// 单步等待上限。
pub const TIMEOUT: Duration = Duration::from_secs(5);

/// 测试用 cookie 值（虚构 openid）。
pub const COOKIE: &str = "openid=test-openid-value";

/// mock faye 的握手成功响应。
pub const HANDSHAKE_OK: &str = r#"[{"id":"1","channel":"/meta/handshake","successful":true,"clientId":"mock-client-id","version":"1.0","supportedConnectionTypes":["websocket"],"advice":{"reconnect":"retry","interval":0,"timeout":15000}}]"#;

/// 第 2 轮二维码推送（`data.type == 1`）。
pub const QR_PUSH: &str = r#"[{"channel":"/attendance/100001/200002/qr","data":{"type":1,"qrUrl":"https://example.test/qr/round2"},"clientId":"mock-client-id"}]"#;

/// 签到关闭推送（`data.type == 2`）。
pub const CLOSED_PUSH: &str =
    r#"[{"channel":"/attendance/100001/200002/qr","data":{"type":2},"clientId":"mock-client-id"}]"#;

/// 前方拥挤推送（`data.type == 3`）。
pub const CONGESTED_PUSH: &str =
    r#"[{"channel":"/attendance/100001/200002/qr","data":{"type":3},"clientId":"mock-client-id"}]"#;

/// 测试用配置：ping 周期压短以便断言，其余取默认值。
#[must_use]
pub fn config() -> Config {
    Config {
        listen_addr: String::from("127.0.0.1:0"),
        ws_ping: Duration::from_millis(80),
        ..Config::defaults()
    }
}

/// 已启动的测试服务。
pub struct TestApp {
    /// 监听地址。
    pub addr: SocketAddr,
    task: JoinHandle<()>,
}

impl TestApp {
    /// HTTP 基地址。
    #[must_use]
    pub fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// `WS /ws` 地址。
    #[must_use]
    pub fn ws_url(&self) -> String {
        format!("ws://{}/ws", self.addr)
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// 启动真实服务：`api_base` 指向 wiremock，`faye_endpoint` 指向 mock faye。
pub async fn spawn_app(config: Config, api_base: &str, faye_endpoint: &str) -> TestResult<TestApp> {
    let api = TeachmateClient::with_base_url(api_base)?;
    let faye = FayePool::new(FayeConfig {
        endpoint: String::from(faye_endpoint),
        heartbeat: config.faye_heartbeat,
        backoff_max: config.reconnect_backoff_max,
    });
    let state = AppState::compose(config, api, faye);
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let task = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, router(state)).await {
            tracing::error!(error = %err, "测试服务异常退出");
        }
    });
    Ok(TestApp { addr, task })
}

/// mock faye 服务器：完成握手后转发收到的帧，并可主动推送。
pub struct MockFaye {
    /// 监听地址。
    pub addr: SocketAddr,
    /// 服务端（mock）收到的客户端帧。
    pub frames: mpsc::Receiver<String>,
    /// 主动推送入口（握手完成后才真正下发）。
    pub push: mpsc::Sender<String>,
}

impl MockFaye {
    /// faye 端点地址。
    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("ws://{}", self.addr)
    }

    /// 在 `window` 内等待含 `needle` 的帧；未等到返回 `None`（其他帧被丢弃）。
    ///
    /// 心跳等无关帧会被跳过，因此"未等到"即表示窗口内没有目标帧。
    pub async fn wait_frame(
        &mut self,
        needle: &str,
        window: Duration,
    ) -> TestResult<Option<String>> {
        let found = tokio::time::timeout(window, async {
            loop {
                match self.frames.recv().await {
                    Some(frame) if frame.contains(needle) => return Some(frame),
                    Some(_) => {}
                    None => return None,
                }
            }
        })
        .await
        .ok();
        Ok(found.flatten())
    }

    /// 等待含 `needle` 的帧；超时或连接关闭视为失败。
    pub async fn expect_frame(&mut self, needle: &str) -> TestResult<String> {
        self.wait_frame(needle, TIMEOUT)
            .await?
            .ok_or_else(|| format!("未收到含 `{needle}` 的帧（mock faye 连接可能已关闭）").into())
    }
}

/// 启动 mock faye（服务于一条连接：握手 → 转发帧 / 接收推送）。
pub async fn mock_faye() -> TestResult<MockFaye> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let (frame_tx, frame_rx) = mpsc::channel(64);
    let (push_tx, mut push_rx) = mpsc::channel(16);
    tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut ws) = accept_async(stream).await else {
            return;
        };
        // 客户端就绪即先发 handshake（预建连语义）。
        let handshake = match ws.next().await {
            Some(Ok(WsMessage::Text(text))) => text.to_string(),
            _ => return,
        };
        if frame_tx.send(handshake).await.is_err() {
            return;
        }
        if ws.send(WsMessage::text(HANDSHAKE_OK)).await.is_err() {
            return;
        }
        loop {
            tokio::select! {
                incoming = ws.next() => match incoming {
                    Some(Ok(WsMessage::Text(text))) => {
                        if frame_tx.send(text.to_string()).await.is_err() {
                            return;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => return,
                },
                Some(frame) = push_rx.recv() => {
                    if ws.send(WsMessage::text(frame)).await.is_err() {
                        return;
                    }
                }
            }
        }
    });
    Ok(MockFaye {
        addr,
        frames: frame_rx,
        push: push_tx,
    })
}

/// 连接 `/ws`；`cookie` 为 `None` 时不带 Cookie 头（模拟未登录）。
pub async fn connect_ws(url: &str, cookie: Option<&str>) -> TestResult<BrowserWs> {
    let uri: Uri = url.parse()?;
    let mut builder = ClientRequestBuilder::new(uri);
    if let Some(cookie) = cookie {
        builder = builder.with_header("Cookie", cookie);
    }
    let (stream, _) = connect_async(builder).await?;
    Ok(stream)
}

/// 读取下一条文本帧（跳过 ping/pong 保活帧）；连接关闭返回 `None`。
pub async fn next_text(ws: &mut BrowserWs, timeout: Duration) -> TestResult<Option<String>> {
    let received = tokio::time::timeout(timeout, async {
        loop {
            match ws.next().await {
                Some(Ok(WsMessage::Text(text))) => return Some(text.to_string()),
                Some(Ok(_)) => {}
                Some(Err(_)) | None => return None,
            }
        }
    })
    .await
    .map_err(|_| String::from("等待文本帧超时"))?;
    Ok(received)
}

/// 等待一个服务端 ping 帧（跳过其他帧）。
pub async fn expect_ping(ws: &mut BrowserWs, timeout: Duration) -> TestResult {
    let found = tokio::time::timeout(timeout, async {
        loop {
            match ws.next().await {
                Some(Ok(WsMessage::Ping(_))) => return true,
                Some(Ok(_)) => {}
                Some(Err(_)) | None => return false,
            }
        }
    })
    .await
    .map_err(|_| String::from("等待 ping 帧超时"))?;
    if found {
        Ok(())
    } else {
        Err(String::from("连接在收到 ping 前已关闭").into())
    }
}

/// 发送一条上行文本帧。
pub async fn send_text(ws: &mut BrowserWs, text: &str) -> TestResult {
    ws.send(WsMessage::text(text)).await?;
    Ok(())
}
