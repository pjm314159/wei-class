//! faye 客户端池：以活跃浏览器连接数驱动单例启停（`docs/DESIGN.md` §3 / §4）。
//!
//! 全局仅一个 `AtomicUsize` 计数 + 一个客户端句柄：
//! - 浏览器连接建立 `fetch_add`，旧值为 `0` → 启动客户端（幂等）并下发
//!   [`Command::Ready`] 触发预建连；
//! - 连接结束 `fetch_sub`，新值为 `0` → 下发 [`Command::Shutdown`] 并丢弃句柄，
//!   下次再有连接时全新预建。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::{broadcast, mpsc, watch};
use tracing::{info, warn};

use super::client::{Command, Event, FayeConfig, spawn};
use crate::config::Config;

/// 默认 `/faye` WebSocket 端点（实测确认，见 `docs/DESIGN.md` §2.1）。
pub const DEFAULT_ENDPOINT: &str = "wss://www.teachermate.com.cn/faye";

/// 一个已启动的客户端句柄（命令发送端 + 事件接收端原型 + 订阅状态）。
struct Handle {
    cmd_tx: mpsc::Sender<Command>,
    event_rx: broadcast::Receiver<Event>,
    sub_rx: watch::Receiver<bool>,
}

/// faye 客户端池：进程内至多一个客户端，随活跃连接数启停。
pub struct FayePool {
    handle: Mutex<Option<Handle>>,
    live: AtomicUsize,
    config: FayeConfig,
}

impl FayePool {
    /// 使用给定 faye 配置创建池（尚无活跃连接，客户端惰性启动）。
    #[must_use]
    pub fn new(config: FayeConfig) -> Self {
        Self {
            handle: Mutex::new(None),
            live: AtomicUsize::new(0),
            config,
        }
    }

    /// 由服务配置创建：心跳与退避上限取自配置，端点用 [`DEFAULT_ENDPOINT`]。
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self::new(FayeConfig {
            endpoint: String::from(DEFAULT_ENDPOINT),
            heartbeat: config.faye_heartbeat,
            backoff_max: config.reconnect_backoff_max,
        })
    }

    /// 当前活跃浏览器连接数。
    #[must_use]
    pub fn live(&self) -> usize {
        self.live.load(Ordering::Acquire)
    }

    /// 取得一条租约：首个租约启动客户端（幂等）并使其预建连就绪。
    ///
    /// 租约被丢弃时计数递减；归零则请求客户端关闭。
    #[must_use]
    pub fn acquire(self: &Arc<Self>) -> Lease {
        let pool = Arc::clone(self);
        self.live.fetch_add(1, Ordering::AcqRel);
        // 临界区内无 await：持锁时间可控，且不跨越挂起点。
        let mut guard = self.handle.lock().unwrap_or_else(PoisonError::into_inner);
        let mut started = false;
        let (cmd_tx, event_rx, sub_rx) = {
            let handle = guard.get_or_insert_with(|| {
                started = true;
                let (cmd_tx, event_rx, sub_rx) = spawn(self.config.clone());
                if let Err(err) = cmd_tx.try_send(Command::Ready) {
                    warn!(error = %err, "faye 就绪命令入队失败");
                }
                Handle {
                    cmd_tx,
                    event_rx,
                    sub_rx,
                }
            });
            (
                handle.cmd_tx.clone(),
                handle.event_rx.resubscribe(),
                handle.sub_rx.clone(),
            )
        };
        drop(guard);
        if started {
            info!("faye 客户端已启动（首条活跃连接）");
        }
        Lease {
            pool,
            cmd_tx,
            event_rx,
            sub_rx,
        }
    }

    /// 关闭客户端：丢弃句柄并下发 [`Command::Shutdown`]（计数归零时调用）。
    fn shutdown(&self) {
        let mut guard = self.handle.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(handle) = guard.take() else {
            return;
        };
        if let Err(err) = handle.cmd_tx.try_send(Command::Shutdown) {
            warn!(error = %err, "faye 关闭命令入队失败");
        }
        info!("活跃连接归零，已请求 faye 客户端关闭");
    }
}

/// 单条浏览器连接持有的 faye 租约：析构时递减计数，归零则关闭客户端。
pub struct Lease {
    pool: Arc<FayePool>,
    cmd_tx: mpsc::Sender<Command>,
    event_rx: broadcast::Receiver<Event>,
    sub_rx: watch::Receiver<bool>,
}

impl Lease {
    /// 取一条独立的事件接收端（从调用时刻起接收后续事件）。
    #[must_use]
    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.event_rx.resubscribe()
    }

    /// 取一条订阅状态接收端：`true` 表示已监听到二维码（订阅表非空）。
    #[must_use]
    pub fn subscribed(&self) -> watch::Receiver<bool> {
        self.sub_rx.clone()
    }

    /// 命令发送端：Poller 用它下发 [`Command::Subscribe`]。
    #[must_use]
    pub fn command(&self) -> &mpsc::Sender<Command> {
        &self.cmd_tx
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if self.pool.live.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.pool.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::net::TcpListener;

    use super::*;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

    /// 占位端点：接受连接并持有，但不完成握手，客户端安静等待。
    async fn idle_endpoint() -> TestResult<String> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move {
            let mut held: Vec<tokio::net::TcpStream> = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        Ok(format!("ws://{addr}"))
    }

    fn pool_of(endpoint: String) -> Arc<FayePool> {
        Arc::new(FayePool::new(FayeConfig {
            endpoint,
            heartbeat: Duration::from_secs(60),
            backoff_max: Duration::from_millis(20),
        }))
    }

    #[tokio::test]
    async fn live_count_follows_leases() -> TestResult {
        let pool = pool_of(idle_endpoint().await?);
        assert_eq!(pool.live(), 0);

        let first = pool.acquire();
        assert_eq!(pool.live(), 1);
        let second = pool.acquire();
        assert_eq!(pool.live(), 2);

        drop(second);
        assert_eq!(pool.live(), 1);
        drop(first);
        assert_eq!(pool.live(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn lease_exposes_command_and_events() -> TestResult {
        let pool = pool_of(idle_endpoint().await?);
        let lease = pool.acquire();
        assert!(lease.command().try_send(Command::Ready).is_ok());

        // 独立接收端：尚无推送时不应就绪。
        let mut events = lease.events();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err()
        );
        Ok(())
    }
}
