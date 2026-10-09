//! 服务配置：从环境变量（可选 `.env` 文件）加载。
//!
//! 解析规则（对应 `docs/DESIGN.md` §9）：
//! - 未设置的键取默认值；
//! - 非法值（非 `u64`、负数、`0`）**回退默认值**并记录告警，而非拒绝启动；
//! - [`Config::min_poll_interval`] 即轮询间隔下限边界：客户端可在边界内自由指定，
//!   服务端在读取点按它钳制（`max`）。

use std::time::Duration;

use tracing::warn;

/// 环境变量键：HTTP/WS 监听地址。
pub const ENV_LISTEN_ADDR: &str = "LISTEN_ADDR";
/// 环境变量键：轮询间隔下限（毫秒）。
pub const ENV_MIN_POLL_INTERVAL_MS: &str = "MIN_POLL_INTERVAL_MS";
/// 环境变量键：faye WebSocket 心跳间隔（毫秒）。
pub const ENV_FAYE_HEARTBEAT_MS: &str = "FAYE_HEARTBEAT_MS";
/// 环境变量键：faye 断线重连退避上限（毫秒）。
pub const ENV_RECONNECT_BACKOFF_MAX_MS: &str = "RECONNECT_BACKOFF_MAX_MS";
/// 环境变量键：浏览器侧 WS 保活 ping 周期（毫秒）。
pub const ENV_WS_PING_MS: &str = "WS_PING_MS";
/// 环境变量键：日志文件目录（按日滚动）。
pub const ENV_LOG_DIR: &str = "LOG_DIR";

/// 服务运行配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// HTTP/WS 监听地址（容器内，如 `127.0.0.1:8080`）。
    pub listen_addr: String,
    /// 轮询间隔下限边界：客户端上报值低于它时在读取点被钳制。
    pub min_poll_interval: Duration,
    /// faye WebSocket 心跳间隔。
    pub faye_heartbeat: Duration,
    /// faye 断线重连退避上限。
    pub reconnect_backoff_max: Duration,
    /// 浏览器侧 WS 保活 ping 周期。
    pub ws_ping: Duration,
    /// 日志文件目录（按日滚动写入，`main` 初始化时使用）。
    pub log_dir: String,
}

impl Config {
    /// 默认配置（对应所有环境变量均未设置）。
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            listen_addr: String::from("127.0.0.1:8080"),
            min_poll_interval: Duration::from_millis(1_000),
            faye_heartbeat: Duration::from_millis(5_000),
            reconnect_backoff_max: Duration::from_secs(30),
            ws_ping: Duration::from_secs(30),
            log_dir: String::from("logs"),
        }
    }

    /// 从进程环境变量加载；`.env` 文件存在时先注入。
    ///
    /// `.env` 缺失属正常部署形态，不告警；加载失败或键值非法均**回退默认值**，
    /// 因此本函数不会失败。
    #[must_use]
    pub fn from_env() -> Self {
        if let Err(err) = dotenvy::dotenv()
            && !err.not_found()
        {
            warn!(error = %err, "`.env` 文件加载失败，已忽略");
        }
        Self::from_env_vars(std::env::vars())
    }

    /// 从键值对集合解析配置（核心纯函数，便于测试）。
    ///
    /// 未知键忽略；已知键的值非法时该项回退默认并告警。
    #[must_use]
    pub fn from_env_vars<I, K, V>(vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut config = Self::defaults();
        for (key, value) in vars {
            let key = key.as_ref();
            let value = value.as_ref();
            match key {
                ENV_LISTEN_ADDR => config.listen_addr = String::from(value),
                ENV_MIN_POLL_INTERVAL_MS => {
                    config.min_poll_interval =
                        parse_duration_ms(value, config.min_poll_interval, key);
                }
                ENV_FAYE_HEARTBEAT_MS => {
                    config.faye_heartbeat = parse_duration_ms(value, config.faye_heartbeat, key);
                }
                ENV_RECONNECT_BACKOFF_MAX_MS => {
                    config.reconnect_backoff_max =
                        parse_duration_ms(value, config.reconnect_backoff_max, key);
                }
                ENV_WS_PING_MS => {
                    config.ws_ping = parse_duration_ms(value, config.ws_ping, key);
                }
                ENV_LOG_DIR => {
                    let value = value.trim();
                    if value.is_empty() {
                        warn!(key, value, "日志目录为空，已回退默认值");
                    } else {
                        config.log_dir = String::from(value);
                    }
                }
                _ => {}
            }
        }
        config
    }
}

/// 解析毫秒数；非法或为 `0` 时回退 `fallback` 并告警。
fn parse_duration_ms(raw: &str, fallback: Duration, key: &str) -> Duration {
    match raw.parse::<u64>() {
        Ok(0) => {
            warn!(key, value = raw, "取值必须为正整数毫秒，已回退默认值");
            fallback
        }
        Ok(ms) => Duration::from_millis(ms),
        Err(_) => {
            warn!(key, value = raw, "无法解析为 `u64` 毫秒数，已回退默认值");
            fallback
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(pairs: &[(&str, &str)]) -> Config {
        Config::from_env_vars(pairs.iter().copied())
    }

    #[test]
    fn empty_vars_yield_defaults() {
        assert_eq!(build(&[]), Config::defaults());
    }

    #[test]
    fn valid_overrides_apply() {
        let config = build(&[
            (ENV_LISTEN_ADDR, "0.0.0.0:9000"),
            (ENV_MIN_POLL_INTERVAL_MS, "2500"),
            (ENV_FAYE_HEARTBEAT_MS, "3000"),
            (ENV_RECONNECT_BACKOFF_MAX_MS, "60000"),
            (ENV_WS_PING_MS, "15000"),
        ]);
        assert_eq!(config.listen_addr, "0.0.0.0:9000");
        assert_eq!(config.min_poll_interval, Duration::from_millis(2_500));
        assert_eq!(config.faye_heartbeat, Duration::from_millis(3_000));
        assert_eq!(config.reconnect_backoff_max, Duration::from_millis(60_000));
        assert_eq!(config.ws_ping, Duration::from_secs(15));
    }

    #[test]
    fn invalid_values_fall_back_to_defaults() {
        let config = build(&[
            (ENV_MIN_POLL_INTERVAL_MS, "abc"),
            (ENV_FAYE_HEARTBEAT_MS, "-5"),
            (ENV_RECONNECT_BACKOFF_MAX_MS, "3.5"),
            (ENV_WS_PING_MS, "0"),
        ]);
        assert_eq!(config, Config::defaults());
    }

    #[test]
    fn partial_override_keeps_other_defaults() {
        let config = build(&[(ENV_MIN_POLL_INTERVAL_MS, "800")]);
        assert_eq!(config.min_poll_interval, Duration::from_millis(800));
        let defaults = Config::defaults();
        assert_eq!(config.faye_heartbeat, defaults.faye_heartbeat);
        assert_eq!(config.listen_addr, defaults.listen_addr);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        assert_eq!(build(&[("SOMETHING_ELSE", "1")]), Config::defaults());
    }
}
