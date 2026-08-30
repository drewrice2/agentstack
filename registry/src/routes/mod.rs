use axum::{Router, routing::get};

use crate::AppState;

pub mod ping;
pub mod registry;
pub mod whoami;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/ping", get(ping::ping))
        .route("/whoami", get(whoami::whoami))
        .merge(registry::router())
}
