use axum::{Json, extract::State};
use serde::Serialize;

use crate::AppState;

#[derive(Debug, Serialize)]
pub struct PingResponse {
    pub status: &'static str,
    pub server_version: String,
    pub build: BuildResponse,
}

#[derive(Debug, Serialize)]
pub struct BuildResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_tag: Option<String>,
}

pub async fn ping(State(state): State<AppState>) -> Json<PingResponse> {
    Json(PingResponse {
        status: "ok",
        server_version: state.server_version,
        build: BuildResponse {
            git_sha: state.build_info.git_sha,
            image_ref: state.build_info.image_ref,
            image_tag: state.build_info.image_tag,
        },
    })
}
