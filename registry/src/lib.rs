pub mod admin;
pub mod audit;
pub mod auth;
pub mod blob_store;
pub mod config;
pub mod db;
pub mod error;
pub(crate) mod registry;
pub mod routes;
pub mod seed;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderValue, header};
use axum::{Router, routing::get};
use blob_store::BlobStore;
use config::QuotaConfig;
use db::DbPool;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::timeout::RequestBodyTimeoutLayer;

const GLOBAL_BODY_LIMIT: usize = 256 * 1024;
const REQUEST_BODY_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, Default)]
pub struct BuildInfo {
    pub git_sha: Option<String>,
    pub image_ref: Option<String>,
    pub image_tag: Option<String>,
}

impl BuildInfo {
    pub fn from_env() -> Self {
        Self {
            git_sha: env_option("AGENTSTACK_SERVER_GIT_SHA"),
            image_ref: env_option("AGENTSTACK_SERVER_IMAGE_REF"),
            image_tag: env_option("AGENTSTACK_SERVER_IMAGE_TAG"),
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub db: DbPool,
    pub blob_store: Arc<dyn BlobStore>,
    pub server_version: String,
    pub build_info: BuildInfo,
    pub quotas: QuotaConfig,
}

impl AppState {
    pub fn new(
        db: DbPool,
        blob_store: Arc<dyn BlobStore>,
        server_version: impl Into<String>,
    ) -> Self {
        Self {
            db,
            blob_store,
            server_version: server_version.into(),
            build_info: BuildInfo::default(),
            quotas: QuotaConfig::default(),
        }
    }

    pub fn with_build_info(mut self, build_info: BuildInfo) -> Self {
        self.build_info = build_info;
        self
    }

    pub fn with_quotas(mut self, quotas: QuotaConfig) -> Self {
        self.quotas = quotas;
        self
    }
}

pub fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(routes::ping::ping))
        .nest("/v1", routes::router())
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(RequestBodyTimeoutLayer::new(REQUEST_BODY_TIMEOUT))
        .layer(DefaultBodyLimit::max(GLOBAL_BODY_LIMIT))
        .with_state(state)
}

fn env_option(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}
