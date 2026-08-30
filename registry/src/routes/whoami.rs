use axum::Json;
use serde::Serialize;

use crate::{
    auth::{AuthenticatedUser, OrgMembership},
    error::ServerError,
};

#[derive(Debug, Serialize)]
pub struct WhoamiResponse {
    pub user: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
    pub email: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub server_admin: bool,
    pub orgs: Vec<OrgMembership>,
}

pub async fn whoami(user: AuthenticatedUser) -> Result<Json<WhoamiResponse>, ServerError> {
    let default_org = user.orgs.first().map(|org| org.slug.clone());
    Ok(Json(WhoamiResponse {
        user: user.email.clone(),
        org: default_org,
        email: user.email,
        name: user.name,
        server_admin: user.is_server_admin,
        orgs: user.orgs,
    }))
}
