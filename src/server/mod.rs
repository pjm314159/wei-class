//! axum 服务层：路由、共享状态与启动入口（`docs/DESIGN.md` §6）。

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use tokio::net::TcpListener;
use tracing::info;

use crate::api::TeachmateClient;
use crate::config::Config;
use crate::error::Error;
use crate::faye::pool::FayePool;

pub mod cookie;
pub mod login;
pub mod ws;

/// 单文件静态页（`GET /`）：M5 前为占位页，前端就绪后直接替换 `static/index.html`。
const INDEX_HTML: &str = include_str!("../../static/index.html");
/// 使用说明页（`GET /usage`）：`OpenID` 获取说明，复刻旧版 `usage.html`。
const USAGE_HTML: &str = include_str!("../../static/usage.html");
/// 站点图标（`GET /favicon.ico`）。
const FAVICON: &[u8] = include_bytes!("../../static/favicon.ico");
/// 前端本地依赖：Alpine.js（`GET /vendor/alpine.min.js`）。
const ALPINE_JS: &[u8] = include_bytes!("../../static/vendor/alpine.min.js");
/// 前端本地依赖：qrcodejs（`GET /vendor/qrcode.min.js`）。
const QRCODE_JS: &[u8] = include_bytes!("../../static/vendor/qrcode.min.js");

/// 服务共享状态（随请求克隆，内部组件均为 `Arc` 或可克隆客户端）。
#[derive(Clone)]
pub struct AppState {
    config: Arc<Config>,
    api: TeachmateClient,
    faye: Arc<FayePool>,
}

impl AppState {
    /// 以默认组件构造：API 客户端用默认基地址，faye 用默认端点。
    ///
    /// # Errors
    /// HTTP 客户端初始化失败（TLS 后端异常）时返回 [`Error::Init`]。
    pub fn new(config: Config) -> Result<Self, Error> {
        let api = TeachmateClient::new().map_err(|err| Error::Init(err.to_string()))?;
        let faye = FayePool::from_config(&config);
        Ok(Self::compose(config, api, faye))
    }

    /// 以指定组件构造（测试用：注入 mock 基地址与 faye 端点）。
    #[must_use]
    pub fn compose(config: Config, api: TeachmateClient, faye: FayePool) -> Self {
        Self {
            config: Arc::new(config),
            api,
            faye: Arc::new(faye),
        }
    }

    /// 服务配置。
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// 微助教 API 客户端。
    #[must_use]
    pub fn api(&self) -> &TeachmateClient {
        &self.api
    }

    /// faye 客户端池。
    #[must_use]
    pub fn faye(&self) -> &Arc<FayePool> {
        &self.faye
    }
}

/// 构建应用路由。
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/usage", get(usage))
        .route("/favicon.ico", get(favicon))
        .route("/vendor/alpine.min.js", get(vendor_alpine))
        .route("/vendor/qrcode.min.js", get(vendor_qrcode))
        .route("/api/login", post(login::login))
        .route("/ws", get(ws::handler))
        .with_state(state)
}

/// 静态页：`GET /`。
async fn index() -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        INDEX_HTML,
    )
        .into_response()
}

/// 使用说明页：`GET /usage`。
async fn usage() -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        USAGE_HTML,
    )
        .into_response()
}

/// 站点图标：`GET /favicon.ico`。
async fn favicon() -> Response {
    ([(header::CONTENT_TYPE, "image/x-icon")], FAVICON).into_response()
}

/// Alpine.js：`GET /vendor/alpine.min.js`。
async fn vendor_alpine() -> Response {
    javascript(ALPINE_JS)
}

/// qrcodejs：`GET /vendor/qrcode.min.js`。
async fn vendor_qrcode() -> Response {
    javascript(QRCODE_JS)
}

/// 以 `application/javascript` 返回脚本内容。
fn javascript(body: &'static [u8]) -> Response {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

/// 绑定监听地址并运行服务（阻塞至进程结束）。
///
/// # Errors
/// 监听地址非法、绑定失败或服务运行出错时返回对应错误。
pub async fn run(config: Config) -> Result<(), Error> {
    let addr: SocketAddr = config
        .listen_addr
        .parse()
        .map_err(|err| Error::Config(format!("LISTEN_ADDR 非法: {err}")))?;
    let listener = TcpListener::bind(addr).await?;
    let state = AppState::new(config)?;
    info!(%addr, "服务监听中");
    axum::serve(listener, router(state)).await?;
    Ok(())
}
