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

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::sleep;
use tracing::{Instrument, debug, debug_span, info, warn};

use crate::api::{ApiError, Coordinates, Sign, SignInOutcome, SignKind, TeachmateClient};
use crate::faye::client::Command;
use crate::protocol::{ServerMessage, SignMode, SignStatus};

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
    /// 下行消息出口（`SessionExpired` / `SignResult` / `LocationRequired`）。
    pub out_tx: mpsc::Sender<ServerMessage>,
    /// 会话结束通知（发送端被丢弃即视为结束）。
    pub stop_rx: oneshot::Receiver<()>,
    /// 当前 GPS 坐标（浏览器热更新）。
    pub location_rx: watch::Receiver<Option<Coordinates>>,
    /// 非二维码签到提交前的基准延迟。
    pub sign_delay: Duration,
}

/// 运行轮询循环；`query` 每轮调用一次，`submit` 为每个非二维码签到调用一次。
///
/// 自动签到常开：普通签到直接提交；GPS 签到使用 [`Deps::location_rx`] 中的坐标，
/// 未配置坐标时先尝试无坐标提交，业务失败后再发 [`ServerMessage::LocationRequired`]。
///
/// 退出条件：会话结束（[`Deps::stop_rx`]）、faye 通道关闭、openid 失效。
pub async fn run_loop<Q, Fut, S, SFut>(deps: Deps, mut query: Q, mut submit: S)
where
    Q: FnMut() -> Fut,
    Fut: Future<Output = Result<Vec<Sign>, ApiError>>,
    S: FnMut(Sign, Option<Coordinates>) -> SFut,
    SFut: Future<Output = Result<SignInOutcome, ApiError>>,
{
    let Deps {
        min,
        mut interval_rx,
        mut subscribed_rx,
        faye_tx,
        out_tx,
        mut stop_rx,
        mut location_rx,
        sign_delay,
    } = deps;
    let mut round: u32 = 0;
    let mut state = RoundState::new();
    let mut force_poll = false;

    loop {
        let forced = std::mem::take(&mut force_poll);
        // 已监听到二维码：暂停轮询，等待签到关闭（订阅清空）或会话结束。
        // 坐标更新时强制查询一轮，避免「填了坐标但轮询已暂停」的卡死。
        if *subscribed_rx.borrow_and_update() && !forced {
            tokio::select! {
                changed = subscribed_rx.changed() => {
                    if changed.is_err() {
                        debug!("faye 客户端已关闭，轮询退出");
                        return;
                    }
                    continue;
                }
                changed = location_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    force_poll = true;
                    continue;
                }
                _ = &mut stop_rx => {
                    debug!("会话结束，轮询退出");
                    return;
                }
            }
        }

        if !forced {
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
                // 坐标热更新：跳过剩余等待，立即查询一轮。
                changed = location_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    force_poll = true;
                    continue;
                }
                _ = &mut stop_rx => {
                    debug!("会话结束，轮询退出");
                    return;
                }
            }
        }

        round += 1;
        if !poll_once(
            &mut query,
            &mut submit,
            &faye_tx,
            &out_tx,
            &location_rx,
            &mut stop_rx,
            &mut state,
            sign_delay,
            round,
        )
        .instrument(debug_span!("poll", round))
        .await
        {
            return;
        }
    }
}

/// 轮询状态：已处理集合、网络失败计数与等待坐标集合。
struct RoundState {
    processed: HashSet<(i64, i64)>,
    attempts: HashMap<(i64, i64), u8>,
    awaiting_location: HashSet<(i64, i64)>,
}

impl RoundState {
    fn new() -> Self {
        Self {
            processed: HashSet::new(),
            attempts: HashMap::new(),
            awaiting_location: HashSet::new(),
        }
    }
}

/// 执行一轮查询并处理结果；返回 `false` 表示会话应结束。
// 依赖的状态与通道较多，保持显式参数以便注入测试实现。
#[allow(clippy::too_many_arguments)]
async fn poll_once<Q, Fut, S, SFut>(
    query: &mut Q,
    submit: &mut S,
    faye_tx: &mpsc::Sender<Command>,
    out_tx: &mpsc::Sender<ServerMessage>,
    location_rx: &watch::Receiver<Option<Coordinates>>,
    stop_rx: &mut oneshot::Receiver<()>,
    state: &mut RoundState,
    sign_delay: Duration,
    round: u32,
) -> bool
where
    Q: FnMut() -> Fut,
    Fut: Future<Output = Result<Vec<Sign>, ApiError>>,
    S: FnMut(Sign, Option<Coordinates>) -> SFut,
    SFut: Future<Output = Result<SignInOutcome, ApiError>>,
{
    match query().await {
        Ok(signs) => {
            // 1) 二维码优先：订阅时延敏感，保持 T0+10s 窗口。
            for sign in signs.iter().filter(|sign| sign.kind() == SignKind::Qr) {
                debug!(
                    course_id = sign.course_id,
                    sign_id = sign.sign_id,
                    "发现二维码签到"
                );
                let command = Command::Subscribe {
                    course_id: sign.course_id,
                    sign_id: sign.sign_id,
                };
                if faye_tx.send(command).await.is_err() {
                    warn!("faye 命令通道已关闭，轮询退出");
                    return false;
                }
            }

            // 2) 普通 / GPS：自动提交（常开）。
            for sign in signs.iter().filter(|sign| sign.kind() != SignKind::Qr) {
                let key = (sign.course_id, sign.sign_id);
                if state.processed.contains(&key) {
                    continue;
                }
                let coords = *location_rx.borrow();
                // 已提示补坐标且仍在等坐标：不重复提交，避免每轮刷请求。
                if sign.kind() == SignKind::Gps
                    && coords.is_none()
                    && state.awaiting_location.contains(&key)
                {
                    continue;
                }
                if !submit_sign(
                    sign,
                    coords,
                    sign_delay,
                    round,
                    &mut state.processed,
                    &mut state.attempts,
                    &mut state.awaiting_location,
                    out_tx,
                    stop_rx,
                    submit,
                )
                .await
                {
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
}

/// 提交一个非二维码签到；返回 `false` 表示会话应结束。
// 状态与通道较多，保持显式参数以便注入测试实现。
#[allow(clippy::too_many_arguments)]
async fn submit_sign<S, SFut>(
    sign: &Sign,
    coords: Option<Coordinates>,
    sign_delay: Duration,
    round: u32,
    processed: &mut HashSet<(i64, i64)>,
    attempts: &mut HashMap<(i64, i64), u8>,
    awaiting_location: &mut HashSet<(i64, i64)>,
    out_tx: &mpsc::Sender<ServerMessage>,
    stop_rx: &mut oneshot::Receiver<()>,
    submit: &mut S,
) -> bool
where
    S: FnMut(Sign, Option<Coordinates>) -> SFut,
    SFut: Future<Output = Result<SignInOutcome, ApiError>>,
{
    let key = (sign.course_id, sign.sign_id);
    let delay = jittered_delay(sign_delay, sign.sign_id, round);
    if !sleep_or_stop(delay, stop_rx).await {
        return false;
    }
    match submit(sign.clone(), coords).await {
        Ok(SignInOutcome::Signed) => {
            processed.insert(key);
            attempts.remove(&key);
            let message = sign_result(sign, SignStatus::Ok, None);
            send_out(out_tx, message).await
        }
        Ok(SignInOutcome::AlreadySigned) => {
            processed.insert(key);
            attempts.remove(&key);
            let message = sign_result(sign, SignStatus::Already, None);
            send_out(out_tx, message).await
        }
        Ok(SignInOutcome::Rejected { code, message }) => {
            // GPS 未配置坐标：失败可能只是缺定位，提示用户补填后重试。
            if sign.kind() == SignKind::Gps && coords.is_none() {
                if awaiting_location.insert(key) {
                    let notice = ServerMessage::LocationRequired {
                        course_id: sign.course_id,
                        sign_id: sign.sign_id,
                        name: sign.name.clone(),
                    };
                    if !send_out(out_tx, notice).await {
                        return false;
                    }
                }
                debug!(
                    course_id = sign.course_id,
                    sign_id = sign.sign_id,
                    "GPS 签到缺坐标，等待用户补填"
                );
                true
            } else {
                processed.insert(key);
                let reason = rejection_reason(code, message);
                let result = sign_result(sign, SignStatus::Failed, Some(reason));
                send_out(out_tx, result).await
            }
        }
        Err(ApiError::TokenInvalid) => {
            let _ = out_tx.send(ServerMessage::SessionExpired).await;
            info!("openid 已失效，轮询终止");
            false
        }
        Err(ApiError::Network(err)) => {
            let count = {
                let entry = attempts.entry(key).or_insert(0);
                *entry += 1;
                *entry
            };
            if count >= 3 {
                processed.insert(key);
                let reason = format!("网络错误: {err}");
                let result = sign_result(sign, SignStatus::Failed, Some(reason));
                send_out(out_tx, result).await
            } else {
                warn!(
                    error = %err,
                    course_id = sign.course_id,
                    sign_id = sign.sign_id,
                    count,
                    "签到提交失败，下轮重试"
                );
                true
            }
        }
    }
}

/// 提交延迟：基准 + 0–1000ms 抖动（基准为 0 时保持立即提交）。
fn jittered_delay(base: Duration, sign_id: i64, round: u32) -> Duration {
    if base.is_zero() {
        return base;
    }
    let seed = sign_id.unsigned_abs() ^ u64::from(round).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    base.saturating_add(Duration::from_millis(seed % 1_001))
}

/// 等待提交延迟；会话结束时返回 `false`。
async fn sleep_or_stop(delay: Duration, stop_rx: &mut oneshot::Receiver<()>) -> bool {
    tokio::select! {
        () = sleep(delay) => true,
        _ = &mut *stop_rx => false,
    }
}

/// 发送下行消息；连接已关闭时返回 `false`。
async fn send_out(out_tx: &mpsc::Sender<ServerMessage>, message: ServerMessage) -> bool {
    out_tx.send(message).await.is_ok()
}

/// 构造签到结果消息。
fn sign_result(sign: &Sign, status: SignStatus, reason: Option<String>) -> ServerMessage {
    ServerMessage::SignResult {
        course_id: sign.course_id,
        sign_id: sign.sign_id,
        name: sign.name.clone(),
        mode: if sign.kind() == SignKind::Gps {
            SignMode::Gps
        } else {
            SignMode::Normal
        },
        status,
        reason,
    }
}

/// 业务失败原因文本。
fn rejection_reason(code: Option<i64>, message: Option<String>) -> String {
    match (code, message) {
        (Some(code), Some(message)) => format!("errorCode={code}: {message}"),
        (Some(code), None) => format!("errorCode={code}"),
        (None, Some(message)) => message,
        (None, None) => String::from("签到被拒绝"),
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
        let query_api = api.clone();
        let query_openid = openid.clone();
        run_loop(
            self.deps,
            move || {
                let api = query_api.clone();
                let openid = query_openid.clone();
                async move { api.active_signs(&openid).await }
            },
            move |sign, coords| {
                let api = api.clone();
                let openid = openid.clone();
                async move {
                    api.sign_in(&openid, sign.course_id, sign.sign_id, coords)
                        .await
                }
            },
        )
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// 假查询的返回类型（装箱后统一形态，便于按轮次喂不同结果）。
    type QueryFuture = std::pin::Pin<Box<dyn Future<Output = Result<Vec<Sign>, ApiError>> + Send>>;

    /// 测试用依赖集合及其配套通道。
    // 一次性返回七个通道，签名较长；仅测试装置使用，故豁免 type_complexity。
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
        watch::Sender<Option<Coordinates>>,
    ) {
        let (faye_tx, faye_rx) = mpsc::channel(16);
        let (out_tx, out_rx) = mpsc::channel(4);
        let (interval_tx, interval_rx) = watch::channel(min);
        let (sub_tx, sub_rx) = watch::channel(false);
        let (location_tx, location_rx) = watch::channel(None);
        let (stop_tx, stop_rx) = oneshot::channel();
        (
            Deps {
                min,
                interval_rx,
                subscribed_rx: sub_rx,
                faye_tx,
                out_tx,
                stop_rx,
                location_rx,
                sign_delay: Duration::ZERO,
            },
            faye_rx,
            out_rx,
            interval_tx,
            sub_tx,
            stop_tx,
            location_tx,
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
            is_gps: 0,
            name: String::from("test"),
        }
    }

    fn gps_sign(course_id: i64, sign_id: i64) -> Sign {
        Sign {
            course_id,
            sign_id,
            is_qr: 0,
            is_gps: 1,
            name: String::from("test-gps"),
        }
    }

    /// 默认提交实现：一律成功。
    fn accept(
        _sign: Sign,
        _coords: Option<Coordinates>,
    ) -> std::future::Ready<Result<SignInOutcome, ApiError>> {
        std::future::ready(Ok(SignInOutcome::Signed))
    }

    /// 等待被 spawn 的轮询任务推进一轮（虚拟时间下让出调度）。
    async fn settle() {
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
    }

    /// 启动轮询任务并先让它完成首次 poll：此时本轮 `sleep` 才完成定时器注册，
    /// 否则虚拟时钟推进会发生在注册之前（经典竞态）。
    async fn start<Q, Fut, S, SFut>(deps: Deps, query: Q, submit: S) -> tokio::task::JoinHandle<()>
    where
        Q: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = Result<Vec<Sign>, ApiError>> + Send + 'static,
        S: FnMut(Sign, Option<Coordinates>) -> SFut + Send + 'static,
        SFut: Future<Output = Result<SignInOutcome, ApiError>> + Send + 'static,
    {
        let handle = tokio::spawn(run_loop(deps, query, submit));
        settle().await;
        handle
    }

    #[tokio::test(start_paused = true)]
    async fn interval_update_applies_from_next_round() -> TestResult {
        let min = Duration::from_secs(1);
        let (deps, _faye_rx, _out_rx, interval_tx, _sub_tx, _stop_tx, _location_tx) = deps_of(min);
        let calls = Arc::new(AtomicUsize::new(0));
        let handle = start(deps, counting(Arc::clone(&calls), Vec::new()), accept).await;

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
        let (deps, _faye_rx, _out_rx, interval_tx, _sub_tx, _stop_tx, _location_tx) = deps_of(min);
        interval_tx.send_replace(Duration::from_millis(200));
        let calls = Arc::new(AtomicUsize::new(0));
        let handle = start(deps, counting(Arc::clone(&calls), Vec::new()), accept).await;

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
    async fn qr_is_subscribed_while_gps_is_submitted() -> TestResult {
        let min = Duration::from_secs(1);
        let (deps, mut faye_rx, _out_rx, _interval_tx, _sub_tx, _stop_tx, _location_tx) =
            deps_of(min);
        let submitted = Arc::new(Mutex::new(Vec::new()));
        let recorder = {
            let submitted = Arc::clone(&submitted);
            move |sign: Sign, coords: Option<Coordinates>| {
                if let Ok(mut list) = submitted.lock() {
                    list.push((sign.sign_id, coords));
                }
                std::future::ready(Ok(SignInOutcome::Signed))
            }
        };
        let handle = start(
            deps,
            counting(
                Arc::new(AtomicUsize::new(0)),
                vec![Ok(vec![
                    qr_sign(100_001, 200_002),
                    gps_sign(300_003, 400_004),
                ])],
            ),
            recorder,
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
        // 非二维码不订阅 faye，但会提交签到。
        assert!(faye_rx.try_recv().is_err());
        let calls = submitted.lock().map_err(|_| String::from("锁中毒"))?;
        assert_eq!(calls.len(), 1, "GPS 签到应被提交一次");
        assert_eq!(calls[0].0, 400_004);
        drop(calls);
        handle.abort();
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn poller_pauses_while_subscribed_and_resumes_after_unsubscribe() -> TestResult {
        let min = Duration::from_secs(1);
        let (deps, mut faye_rx, _out_rx, _interval_tx, sub_tx, _stop_tx, _location_tx) =
            deps_of(min);
        let calls = Arc::new(AtomicUsize::new(0));
        let handle = start(
            deps,
            counting(
                Arc::clone(&calls),
                vec![Ok(vec![qr_sign(100_001, 200_002)])],
            ),
            accept,
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
        let (deps, _faye_rx, mut out_rx, _interval_tx, _sub_tx, _stop_tx, _location_tx) =
            deps_of(min);
        let handle = start(
            deps,
            counting(
                Arc::new(AtomicUsize::new(0)),
                vec![Err(ApiError::TokenInvalid)],
            ),
            accept,
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
        let (deps, _faye_rx, mut out_rx, _interval_tx, _sub_tx, _stop_tx, _location_tx) =
            deps_of(min);
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
            accept,
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
        let (deps, _faye_rx, _out_rx, _interval_tx, _sub_tx, stop_tx, _location_tx) = deps_of(min);
        let handle = start(
            deps,
            counting(Arc::new(AtomicUsize::new(0)), Vec::new()),
            accept,
        )
        .await;

        let _ = stop_tx.send(());
        tokio::time::timeout(Duration::from_secs(1), handle).await??;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn gps_without_location_requests_coordinates_then_retries() -> TestResult {
        let min = Duration::from_secs(1);
        let (deps, _faye_rx, mut out_rx, _interval_tx, _sub_tx, _stop_tx, location_tx) =
            deps_of(min);
        let submitted = Arc::new(Mutex::new(Vec::new()));
        let recorder = {
            let submitted = Arc::clone(&submitted);
            move |sign: Sign, coords: Option<Coordinates>| {
                let accepted = coords.is_some();
                if let Ok(mut list) = submitted.lock() {
                    list.push((sign.sign_id, coords));
                }
                if accepted {
                    std::future::ready(Ok(SignInOutcome::Signed))
                } else {
                    std::future::ready(Ok(SignInOutcome::Rejected {
                        code: Some(4_003),
                        message: Some(String::from("location required")),
                    }))
                }
            }
        };
        let handle = start(
            deps,
            counting(
                Arc::new(AtomicUsize::new(0)),
                vec![
                    Ok(vec![gps_sign(1, 2)]),
                    Ok(vec![gps_sign(1, 2)]),
                    Ok(vec![gps_sign(1, 2)]),
                    Ok(vec![gps_sign(1, 2)]),
                ],
            ),
            recorder,
        )
        .await;

        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        let notice = tokio::time::timeout(Duration::from_secs(1), out_rx.recv())
            .await?
            .ok_or_else(|| String::from("应收到 LocationRequired"))?;
        assert!(
            matches!(notice, ServerMessage::LocationRequired { sign_id: 2, .. }),
            "notice = {notice:?}"
        );

        // 等待坐标期间不应每轮重复提交或重复提示。
        tokio::time::advance(Duration::from_secs(2)).await;
        settle().await;
        assert_eq!(
            submitted.lock().map_err(|_| String::from("锁中毒"))?.len(),
            1,
            "等待坐标期间不应重复提交"
        );
        assert!(
            out_rx.try_recv().is_err(),
            "等待坐标期间不应重复发送 LocationRequired"
        );

        location_tx.send_replace(Some(Coordinates {
            lon: 114.440_465,
            lat: 30.517_877,
        }));
        let result = tokio::time::timeout(Duration::from_secs(1), out_rx.recv())
            .await?
            .ok_or_else(|| String::from("坐标补齐后应收到 SignResult"))?;
        assert!(
            matches!(
                result,
                ServerMessage::SignResult {
                    status: SignStatus::Ok,
                    ..
                }
            ),
            "result = {result:?}"
        );

        let calls = submitted.lock().map_err(|_| String::from("锁中毒"))?;
        assert_eq!(calls.len(), 2, "应先无坐标失败、再带坐标成功");
        assert!(calls[0].1.is_none());
        assert!(calls[1].1.is_some());
        drop(calls);
        handle.abort();
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn signed_sign_is_not_resubmitted() -> TestResult {
        let min = Duration::from_secs(1);
        let (deps, _faye_rx, _out_rx, _interval_tx, _sub_tx, _stop_tx, location_tx) = deps_of(min);
        location_tx.send_replace(Some(Coordinates {
            lon: 114.440_465,
            lat: 30.517_877,
        }));
        let submitted = Arc::new(Mutex::new(Vec::new()));
        let recorder = {
            let submitted = Arc::clone(&submitted);
            move |sign: Sign, coords: Option<Coordinates>| {
                if let Ok(mut list) = submitted.lock() {
                    list.push((sign.sign_id, coords));
                }
                std::future::ready(Ok(SignInOutcome::Signed))
            }
        };
        let handle = start(
            deps,
            counting(
                Arc::new(AtomicUsize::new(0)),
                vec![
                    Ok(vec![gps_sign(1, 2)]),
                    Ok(vec![gps_sign(1, 2)]),
                    Ok(vec![gps_sign(1, 2)]),
                ],
            ),
            recorder,
        )
        .await;

        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(
            submitted.lock().map_err(|_| String::from("锁中毒"))?.len(),
            1
        );

        tokio::time::advance(Duration::from_secs(2)).await;
        settle().await;
        assert_eq!(
            submitted.lock().map_err(|_| String::from("锁中毒"))?.len(),
            1,
            "同一签到不应重复提交"
        );
        handle.abort();
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn location_change_while_paused_forces_one_poll() -> TestResult {
        let min = Duration::from_secs(1);
        let (deps, _faye_rx, mut out_rx, _interval_tx, sub_tx, _stop_tx, location_tx) =
            deps_of(min);
        // 模拟已订阅二维码：Poller 进入暂停。
        sub_tx.send_replace(true);
        let handle = start(
            deps,
            counting(
                Arc::new(AtomicUsize::new(0)),
                vec![Ok(vec![gps_sign(1, 2)])],
            ),
            accept,
        )
        .await;
        settle().await;

        // 暂停期间补坐标：应强制查询一轮并提交。
        location_tx.send_replace(Some(Coordinates {
            lon: 114.440_465,
            lat: 30.517_877,
        }));
        let result = tokio::time::timeout(Duration::from_secs(1), out_rx.recv())
            .await?
            .ok_or_else(|| String::from("暂停期间补坐标应触发提交"))?;
        assert!(
            matches!(
                result,
                ServerMessage::SignResult {
                    status: SignStatus::Ok,
                    ..
                }
            ),
            "result = {result:?}"
        );
        handle.abort();
        Ok(())
    }
}
