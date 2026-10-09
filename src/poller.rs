//! SignPoller：每连接一个的签到轮询任务（`docs/DESIGN.md` §5）。
//!
//! ```text
//! loop {
//!     已监听到二维码（订阅表非空）→ 暂停轮询，等待 type:2 退订后恢复
//!     间隔以轮读取（按下限钳制）→ sleep
//!     Ok(signs)   → 二维码签到逐条下发 faye Subscribe（去重在 faye 订阅表）
//!     TokenInvalid → 下发 SessionExpired 并结束
//!     Network      → 本轮放弃，下轮重试
//! }
//! ```
//!
//! 暂停语义（2026-10-09 产品决策）：监听到二维码后不再查询 `active_signs`，
//! 省 API、避风控；代价是监听 A 课期间不会发现 B 课新开的签。
//!
//! 循环骨架 [`run_loop`] 与数据源解耦：真实数据源由 [`Poller`] 注入，
//! 测试注入假查询并用 `tokio::time::pause` 精确控制 tick。

use std::future::Future;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::sleep;
use tracing::{Instrument, debug, debug_span, info, warn};

use crate::api::{ApiError, Sign, TeachmateClient};
use crate::faye::client::Command;
use crate::protocol::ServerMessage;

/// 轮询依赖：循环骨架所需的通道与间隔边界。
pub struct Deps {
    /// 轮询间隔下限（读取点钳制）。
    pub min: Duration,
    /// 客户端上报的间隔（每轮读取一次，热更新下一轮生效）。
    pub interval_rx: watch::Receiver<Duration>,
    /// faye 订阅状态：`true` 表示已监听到二维码（暂停轮询）。
    pub subscribed_rx: watch::Receiver<bool>,
    /// faye 命令出口（发现签到 → [`Command::Subscribe`]）。
    pub faye_tx: mpsc::Sender<Command>,
    /// 下行消息出口（openid 失效 → [`ServerMessage::SessionExpired`]）。
    pub out_tx: mpsc::Sender<ServerMessage>,
    /// 会话结束通知（发送端被丢弃即视为结束）。
    pub stop_rx: oneshot::Receiver<()>,
}

/// 运行轮询循环；`query` 每轮调用一次。
///
/// 退出条件：会话结束（[`Deps::stop_rx`]）、faye 通道关闭、openid 失效。
pub async fn run_loop<Q, Fut>(deps: Deps, mut query: Q)
where
    Q: FnMut() -> Fut,
    Fut: Future<Output = Result<Vec<Sign>, ApiError>>,
{
    let Deps {
        min,
        mut interval_rx,
        mut subscribed_rx,
        faye_tx,
        out_tx,
        mut stop_rx,
    } = deps;
    let mut round: u32 = 0;
    loop {
        // 已监听到二维码：暂停轮询，等待签到关闭（订阅清空）或会话结束。
        if *subscribed_rx.borrow_and_update() {
            tokio::select! {
                changed = subscribed_rx.changed() => {
                    if changed.is_err() {
                        debug!("faye 客户端已关闭，轮询退出");
                        return;
                    }
                    continue;
                }
                _ = &mut stop_rx => {
                    debug!("会话结束，轮询退出");
                    return;
                }
            }
        }

        let wait = effective_interval(&mut interval_rx, min);
        tokio::select! {
            () = sleep(wait) => {}
            // 等待期间发现签到（faye 置位订阅状态）：立即中断本轮等待进入暂停。
            changed = subscribed_rx.changed(), if !*subscribed_rx.borrow() => {
                if changed.is_err() {
                    debug!("faye 客户端已关闭，轮询退出");
                    return;
                }
                continue;
            }
            _ = &mut stop_rx => {
                debug!("会话结束，轮询退出");
                return;
            }
        }

        round += 1;
        let polled = async {
            match query().await {
                Ok(signs) => {
                    let discovered: Vec<(i64, i64)> = signs
                        .iter()
                        .filter(|sign| sign.is_qr != 0)
                        .map(|sign| (sign.course_id, sign.sign_id))
                        .collect();
                    for (course_id, sign_id) in discovered {
                        debug!(course_id, sign_id, "发现二维码签到");
                        let command = Command::Subscribe { course_id, sign_id };
                        if faye_tx.send(command).await.is_err() {
                            warn!("faye 命令通道已关闭，轮询退出");
                            return false;
                        }
                    }
                    true
                }
                Err(ApiError::TokenInvalid) => {
                    let _ = out_tx.send(ServerMessage::SessionExpired).await;
                    info!("openid 已失效，轮询终止");
                    false
                }
                Err(ApiError::Network(err)) => {
                    warn!(error = %err, "本轮查询失败，下轮重试");
                    true
                }
            }
        };
        // `false` 表示轮询应终止（openid 失效或 faye 通道关闭）。
        if !polled.instrument(debug_span!("poll", round)).await {
            return;
        }
    }
}

/// 一条浏览器连接的签到轮询：绑定 openid 与 API 客户端。
pub struct Poller {
    openid: String,
    api: TeachmateClient,
    deps: Deps,
}

impl Poller {
    /// 组装轮询任务。
    #[must_use]
    pub fn new(openid: String, api: TeachmateClient, deps: Deps) -> Self {
        Self { openid, api, deps }
    }

    /// 运行到会话结束、通道关闭或 openid 失效。
    pub async fn run(self) {
        let api = self.api;
        let openid = self.openid;
        run_loop(self.deps, move || {
            let api = api.clone();
            let openid = openid.clone();
            async move { api.active_signs(&openid).await }
        })
        .await;
    }
}

/// 读取当前生效间隔：按服务端下限钳制（`docs/DESIGN.md` §5 的读取点语义）。
pub fn effective_interval(interval_rx: &mut watch::Receiver<Duration>, min: Duration) -> Duration {
    clamp(*interval_rx.borrow_and_update(), min)
}

/// 按下限钳制间隔（唯一实现点，供轮询与 `Status` 回显共用）。
#[must_use]
pub fn clamp(interval: Duration, min: Duration) -> Duration {
    interval.max(min)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// 假查询的返回类型（装箱后统一形态，便于按轮次喂不同结果）。
    type QueryFuture = std::pin::Pin<Box<dyn Future<Output = Result<Vec<Sign>, ApiError>> + Send>>;

    /// 测试用依赖集合及其配套通道。
    // 一次性返回六个通道，签名较长；仅测试装置使用，故豁免 type_complexity。
    #[allow(clippy::type_complexity)]
    fn deps_of(
        min: Duration,
    ) -> (
        Deps,
        mpsc::Receiver<Command>,
        mpsc::Receiver<ServerMessage>,
        watch::Sender<Duration>,
        watch::Sender<bool>,
        oneshot::Sender<()>,
    ) {
        let (faye_tx, faye_rx) = mpsc::channel(16);
        let (out_tx, out_rx) = mpsc::channel(4);
        let (interval_tx, interval_rx) = watch::channel(min);
        let (sub_tx, sub_rx) = watch::channel(false);
        let (stop_tx, stop_rx) = oneshot::channel();
        (
            Deps {
                min,
                interval_rx,
                subscribed_rx: sub_rx,
                faye_tx,
                out_tx,
                stop_rx,
            },
            faye_rx,
            out_rx,
            interval_tx,
            sub_tx,
            stop_tx,
        )
    }

    /// 计数查询：`results` 按轮次返回结果，用尽后返回空列表。
    fn counting(
        calls: Arc<AtomicUsize>,
        mut results: Vec<Result<Vec<Sign>, ApiError>>,
    ) -> impl FnMut() -> QueryFuture {
        move || {
            let calls = Arc::clone(&calls);
            let result = if results.is_empty() {
                Ok(Vec::new())
            } else {
                results.remove(0)
            };
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                result
            })
        }
    }

    fn qr_sign(course_id: i64, sign_id: i64) -> Sign {
        Sign {
            course_id,
            sign_id,
            is_qr: 1,
            name: String::from("test"),
        }
    }

    /// 等待被 spawn 的轮询任务推进一轮（虚拟时间下让出调度）。
    async fn settle() {
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
    }

    /// 启动轮询任务并先让它完成首次 poll：此时本轮 `sleep` 才完成定时器注册，
    /// 否则虚拟时钟推进会发生在注册之前（经典竞态）。
    async fn start<Q, Fut>(deps: Deps, query: Q) -> tokio::task::JoinHandle<()>
    where
        Q: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = Result<Vec<Sign>, ApiError>> + Send + 'static,
    {
        let handle = tokio::spawn(run_loop(deps, query));
        settle().await;
        handle
    }

    #[tokio::test(start_paused = true)]
    async fn interval_update_applies_from_next_round() -> TestResult {
        let min = Duration::from_secs(1);
        let (deps, _faye_rx, _out_rx, interval_tx, _sub_tx, _stop_tx) = deps_of(min);
        let calls = Arc::new(AtomicUsize::new(0));
        let handle = start(deps, counting(Arc::clone(&calls), Vec::new())).await;

        // 第一轮 sleep 进行中改值：当前轮仍按旧间隔结束。
        interval_tx.send_replace(Duration::from_secs(3));
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "当前轮应沿用旧间隔");

        // 下一轮起按新间隔：再推进 1s 不应触发。
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "新间隔应在下一轮生效");

        tokio::time::advance(Duration::from_secs(2)).await;
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        handle.abort();
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn reported_interval_below_minimum_is_clamped() -> TestResult {
        let min = Duration::from_secs(1);
        let (deps, _faye_rx, _out_rx, interval_tx, _sub_tx, _stop_tx) = deps_of(min);
        interval_tx.send_replace(Duration::from_millis(200));
        let calls = Arc::new(AtomicUsize::new(0));
        let handle = start(deps, counting(Arc::clone(&calls), Vec::new())).await;

        tokio::time::advance(Duration::from_millis(500)).await;
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 0, "低于下限的上报值应被钳制");

        tokio::time::advance(Duration::from_millis(500)).await;
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        handle.abort();
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn discovered_qr_signs_are_subscribed_and_gps_skipped() -> TestResult {
        let min = Duration::from_secs(1);
        let (deps, mut faye_rx, _out_rx, _interval_tx, _sub_tx, _stop_tx) = deps_of(min);
        let mut gps = qr_sign(300_003, 400_004);
        gps.is_qr = 0;
        let handle = start(
            deps,
            counting(
                Arc::new(AtomicUsize::new(0)),
                vec![Ok(vec![qr_sign(100_001, 200_002), gps])],
            ),
        )
        .await;

        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        let command = faye_rx.try_recv()?;
        assert_eq!(
            command,
            Command::Subscribe {
                course_id: 100_001,
                sign_id: 200_002
            }
        );
        // GPS 签到不订阅：本轮仅一条命令。
        assert!(faye_rx.try_recv().is_err());
        handle.abort();
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn poller_pauses_while_subscribed_and_resumes_after_unsubscribe() -> TestResult {
        let min = Duration::from_secs(1);
        let (deps, mut faye_rx, _out_rx, _interval_tx, sub_tx, _stop_tx) = deps_of(min);
        let calls = Arc::new(AtomicUsize::new(0));
        let handle = start(
            deps,
            counting(
                Arc::clone(&calls),
                vec![Ok(vec![qr_sign(100_001, 200_002)])],
            ),
        )
        .await;

        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            faye_rx.try_recv()?,
            Command::Subscribe {
                course_id: 100_001,
                sign_id: 200_002
            }
        );

        // 模拟 faye 已订阅：轮询应暂停。
        let _ = sub_tx.send(true);
        tokio::time::advance(Duration::from_secs(3)).await;
        settle().await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "已订阅到二维码时应暂停轮询"
        );

        // 签到关闭退订 → 订阅清空 → 恢复轮询（下一轮间隔后再次查询）。
        // 先 settle 让暂停分支醒来并注册下一轮 sleep，再推进时钟。
        let _ = sub_tx.send(false);
        settle().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2, "退订后应恢复轮询");

        handle.abort();
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn token_invalid_pushes_session_expired_and_stops() -> TestResult {
        let min = Duration::from_secs(1);
        let (deps, _faye_rx, mut out_rx, _interval_tx, _sub_tx, _stop_tx) = deps_of(min);
        let handle = start(
            deps,
            counting(
                Arc::new(AtomicUsize::new(0)),
                vec![Err(ApiError::TokenInvalid)],
            ),
        )
        .await;

        tokio::time::advance(Duration::from_secs(1)).await;
        let message = out_rx.recv().await;
        assert_eq!(message, Some(ServerMessage::SessionExpired));

        // 轮询已终止：任务自行结束。
        tokio::time::timeout(Duration::from_secs(1), handle).await??;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn network_error_skips_round_and_continues() -> TestResult {
        let min = Duration::from_secs(1);
        let (deps, _faye_rx, mut out_rx, _interval_tx, _sub_tx, _stop_tx) = deps_of(min);
        let calls = Arc::new(AtomicUsize::new(0));
        let handle = start(
            deps,
            counting(
                Arc::clone(&calls),
                vec![
                    Err(ApiError::Network(String::from("boom"))),
                    Err(ApiError::Network(String::from("boom"))),
                ],
            ),
        )
        .await;

        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;

        assert_eq!(calls.load(Ordering::SeqCst), 2, "网络错误只跳过本轮");
        assert!(
            out_rx.try_recv().is_err(),
            "网络错误不应下发 SessionExpired"
        );
        assert!(!handle.is_finished());
        handle.abort();
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn session_close_stops_polling() -> TestResult {
        let min = Duration::from_secs(1);
        let (deps, _faye_rx, _out_rx, _interval_tx, _sub_tx, stop_tx) = deps_of(min);
        let handle = start(deps, counting(Arc::new(AtomicUsize::new(0)), Vec::new())).await;

        let _ = stop_tx.send(());
        tokio::time::timeout(Duration::from_secs(1), handle).await??;
        Ok(())
    }
}
