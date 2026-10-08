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
