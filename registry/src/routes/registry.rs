use std::{collections::BTreeSet, sync::Arc};

use axum::extract::Request;
use axum::{
    Json, Router,
    body::Body,
    extract::{
        DefaultBodyLimit, FromRequest, FromRequestParts, Multipart, Path, Query, State,
        multipart::MultipartRejection,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CONTENT_DISPOSITION, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS},
        request::Parts,
    },
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, put},
};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;

use crate::{
    AppState,
    audit::{AuditEvent, list_events as list_audit_events},
    auth::AuthenticatedUser,
    blob_store::BlobStoreError,
    config::QuotaConfig,
    db::DbPool,
    error::{ServerError, map_sql},
    registry::{
        archive::validate_archive_metadata_blocking,
        authz::{
            AccessRole, PermissionDenied, can_publish_visibility, can_read_version,
            can_read_visibility, is_team_admin_role, require_role,
        },
        queries::{
            SkillCatalogFilters, fetch_metadata, latest_skills_with_filters, parse_version_number,
            visibility_from_db, visible_skill_summary, visible_versions,
        },
        types::{
            CatalogSort, PackageHash, RemoteSkill, SkillMetadata, VersionInfo, VersionStatus,
            Visibility,
        },
        validate_slug,
    },
};

const MAX_ARCHIVE_BYTES: usize = 50 * 1024 * 1024;
const MAX_HTTP_AUDIT_EVENTS: i64 = 500;

struct RegistryJson<T>(T);

impl<S, T> FromRequest<S> for RegistryJson<T>
where
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
    S: Send + Sync,
{
    type Rejection = ServerError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        Json::<T>::from_request(req, state)
            .await
            .map(|Json(value)| Self(value))
            .map_err(map_json_rejection)
    }
}

struct RegistryQuery<T>(T);

impl<S, T> FromRequestParts<S> for RegistryQuery<T>
where
    Query<T>: FromRequestParts<S, Rejection = QueryRejection>,
    S: Send + Sync,
{
    type Rejection = ServerError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(value)| Self(value))
            .map_err(map_query_rejection)
    }
}

struct RegistryMultipart(Multipart);

impl<S> FromRequest<S> for RegistryMultipart
where
    Multipart: FromRequest<S, Rejection = MultipartRejection>,
    S: Send + Sync,
{
    type Rejection = ServerError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        Multipart::from_request(req, state)
            .await
            .map(Self)
            .map_err(map_multipart_rejection)
    }
}

fn map_json_rejection(rejection: JsonRejection) -> ServerError {
    if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return ServerError::payload_too_large("request body exceeds size limit");
    }
    ServerError::bad_request("request body is not valid JSON")
}

fn map_query_rejection(_rejection: QueryRejection) -> ServerError {
    // A query-string rejection is always a deserialize/UTF-8 failure, never 413.
    ServerError::bad_request("invalid query string")
}

fn map_multipart_rejection(rejection: MultipartRejection) -> ServerError {
    if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return ServerError::payload_too_large("request body exceeds size limit");
    }
    ServerError::bad_request("malformed multipart request")
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/skills", get(list_all_skills))
        .route("/search", get(search_skills))
        .route("/orgs/{org}/skills", post(push_skill).get(list_org_skills))
        .route("/orgs/{org}/skills/{skill}", get(get_latest_skill))
        .route("/orgs/{org}/skills/{skill}/status", get(skill_status))
        .route("/orgs/{org}/skills/{skill}/impact", get(skill_impact))
        .route("/orgs/{org}/skills/{skill}/audit", get(skill_audit))
        .route(
            "/orgs/{org}/skills/{skill}/visibility",
            get(skill_visibility).patch(patch_skill_visibility),
        )
        .route(
            "/orgs/{org}/skills/{skill}/versions",
            get(list_skill_versions),
        )
        .route(
            "/orgs/{org}/skills/{skill}/versions/{version}",
            get(get_skill_version),
        )
        .route(
            "/orgs/{org}/skills/{skill}/versions/{version}/approve",
            post(approve_skill_version),
        )
        .route(
            "/orgs/{org}/skills/{skill}/versions/{version}/yank",
            post(yank_skill_version),
        )
        .route(
            "/orgs/{org}/skills/{skill}/versions/{version}/deprecate",
            post(deprecate_skill_version),
        )
        .route(
            "/orgs/{org}/skills/{skill}/versions/{version}/archive",
            get(get_skill_archive),
        )
        .route("/orgs/{org}/stacks", post(create_stack).get(list_stacks))
        .route(
            "/orgs/{org}/stacks/{stack}",
            get(get_stack).patch(patch_stack),
        )
        .route(
            "/orgs/{org}/stacks/{stack}/visibility",
            patch(patch_stack_visibility),
        )
        .route("/orgs/{org}/stacks/{stack}/status", get(stack_status))
        .route("/orgs/{org}/stacks/{stack}/audit", get(stack_audit))
        .route("/orgs/{org}/stacks/{stack}/items", post(upsert_stack_item))
        .route(
            "/orgs/{org}/stacks/{stack}/items/{skill}",
            delete(delete_stack_item),
        )
        .route("/orgs/{org}/stacks/{stack}/resolve", get(resolve_stack))
        .route("/orgs/{org}/teams", post(create_team).get(list_teams))
        .route("/orgs/{org}/audit", get(org_audit))
        .route("/orgs/{org}/audit/{event_id}", get(org_audit_event))
        .route("/orgs/{org}/teams/{team}", get(inspect_team))
        .route(
            "/orgs/{org}/teams/{team}/members/{email}",
            put(add_team_member)
                .delete(remove_team_member)
                .patch(set_team_member_role),
        )
        .layer(DefaultBodyLimit::max(MAX_ARCHIVE_BYTES + 1024 * 1024))
}

#[derive(Debug, Serialize)]
struct TeamSummary {
    org: String,
    slug: String,
}

#[derive(Debug, Serialize)]
struct TeamMember {
    email: String,
    role: String,
}

#[derive(Debug, Serialize)]
struct TeamDetail {
    org: String,
    slug: String,
    members: Vec<TeamMember>,
}

#[derive(Debug, Serialize)]
struct TeamListEnvelope {
    teams: Vec<TeamSummary>,
}

#[derive(Debug, Serialize)]
struct TeamEnvelope {
    team: TeamDetail,
    #[serde(skip_serializing_if = "Option::is_none")]
    audit_event_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateTeamBody {
    slug: String,
}

#[derive(Debug, Deserialize)]
struct TeamRoleBody {
    role: String,
}

fn parse_team_role(value: &str) -> Result<&'static str, ServerError> {
    match value {
        "member" => Ok("member"),
        "team_admin" | "lead" => Ok("team_admin"),
        other => Err(ServerError::validation_error(format!(
            "unknown team role `{other}` (expected one of: member, team_admin; legacy lead is accepted)"
        ))),
    }
}

async fn create_team(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(org): Path<String>,
    RegistryJson(body): RegistryJson<CreateTeamBody>,
) -> Result<(StatusCode, Json<TeamEnvelope>), ServerError> {
    validate_slug(&org).map_err(ServerError::validation_error)?;
    let role = require_role(&user, &org, AccessRole::OrgAdmin).map_err(permission_denied)?;
    validate_slug(&body.slug).map_err(ServerError::validation_error)?;

    let mut tx = state.db.begin().await.map_err(map_sql)?;
    let org_id: Option<String> =
        sqlx::query_scalar("SELECT id FROM orgs WHERE slug = $1 FOR UPDATE")
            .bind(&org)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sql)?;
    let org_id =
        org_id.ok_or_else(|| ServerError::validation_error(format!("unknown org `{org}`")))?;

    let exists: Option<String> =
        sqlx::query_scalar("SELECT id FROM teams WHERE org_id = $1 AND slug = $2")
            .bind(&org_id)
            .bind(&body.slug)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sql)?;
    if exists.is_some() {
        return Err(ServerError::validation_error(format!(
            "team `{}/{}` already exists",
            org, body.slug
        )));
    }

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM teams WHERE org_id = $1")
        .bind(&org_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_sql)?;
    if count >= state.quotas.max_teams_per_org {
        return Err(ServerError::quota_exceeded(format!(
            "org `{org}` has reached the team limit of {}",
            state.quotas.max_teams_per_org
        )));
    }

    ensure_creator_can_be_team_admin(&mut tx, &user, &org_id, &org).await?;

    let id = random_id("tm");
    sqlx::query(
        "INSERT INTO teams (id, org_id, slug, name, created_at, updated_at)
         VALUES ($1, $2, $3, $3, now(), now())",
    )
    .bind(&id)
    .bind(&org_id)
    .bind(&body.slug)
    .execute(&mut *tx)
    .await
    .map_err(map_sql)?;
    sqlx::query(
        "INSERT INTO team_memberships (team_id, org_id, user_id, role, created_at, updated_at)
         VALUES ($1, $2, $3, 'team_admin', now(), now())",
    )
    .bind(&id)
    .bind(&org_id)
    .bind(&user.id)
    .execute(&mut *tx)
    .await
    .map_err(map_sql)?;
    let audit_event_id = insert_audit_log(
        &mut tx,
        &org_id,
        &user.id,
        "team",
        &id,
        "team.created",
        serde_json::json!({
            "team": &body.slug,
            "role": role.as_str(),
        }),
    )
    .await?;
    tx.commit().await.map_err(map_sql)?;

    let detail = load_team_detail(&state.db, &org, &body.slug).await?;
    Ok((
        StatusCode::CREATED,
        Json(TeamEnvelope {
            team: detail,
            audit_event_id: Some(audit_event_id),
        }),
    ))
}

async fn ensure_creator_can_be_team_admin(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user: &AuthenticatedUser,
    org_id: &str,
    org: &str,
) -> Result<(), ServerError> {
    let existing: Option<String> =
        sqlx::query_scalar("SELECT user_id FROM org_members WHERE org_id = $1 AND user_id = $2")
            .bind(org_id)
            .bind(&user.id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(map_sql)?;
    if existing.is_some() {
        return Ok(());
    }

    if !user.is_server_admin {
        tracing::error!(
            user_id = %user.id,
            org = %org,
            "org-admin role resolved without an org membership"
        );
        return Err(ServerError::internal_error());
    }

    Err(ServerError::forbidden(format!(
        "server admin must be an org member before creating a team in `{org}`"
    )))
}

async fn list_teams(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(org): Path<String>,
) -> Result<Json<TeamListEnvelope>, ServerError> {
    validate_slug(&org).map_err(ServerError::validation_error)?;
    let role = require_role(&user, &org, AccessRole::Reader).map_err(permission_denied)?;
    let org_id = org_id_by_slug(&state.db, &org)
        .await?
        .ok_or_else(|| ServerError::validation_error(format!("unknown org `{org}`")))?;

    let rows = if role >= AccessRole::OrgAdmin {
        sqlx::query("SELECT slug FROM teams WHERE org_id = $1 ORDER BY slug ASC")
            .bind(&org_id)
            .fetch_all(&state.db)
            .await
            .map_err(map_sql)?
    } else {
        sqlx::query(
            "SELECT teams.slug
             FROM teams
             JOIN team_memberships ON team_memberships.team_id = teams.id
             WHERE teams.org_id = $1 AND team_memberships.user_id = $2
             ORDER BY teams.slug ASC",
        )
        .bind(&org_id)
        .bind(&user.id)
        .fetch_all(&state.db)
        .await
        .map_err(map_sql)?
    };

    let teams = rows
        .into_iter()
        .map(|row| TeamSummary {
            org: org.clone(),
            slug: row.get("slug"),
        })
        .collect();
    Ok(Json(TeamListEnvelope { teams }))
}

async fn inspect_team(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((org, team)): Path<(String, String)>,
) -> Result<Json<TeamEnvelope>, ServerError> {
    validate_slug(&org).map_err(ServerError::validation_error)?;
    validate_slug(&team).map_err(ServerError::validation_error)?;
    let role = require_role(&user, &org, AccessRole::Reader).map_err(permission_denied)?;

    let org_id = org_id_by_slug(&state.db, &org)
        .await?
        .ok_or_else(|| ServerError::validation_error(format!("unknown org `{org}`")))?;
    let team_id: Option<String> =
        sqlx::query_scalar("SELECT id FROM teams WHERE org_id = $1 AND slug = $2")
            .bind(&org_id)
            .bind(&team)
            .fetch_optional(&state.db)
            .await
            .map_err(map_sql)?;
    let team_id = team_id
        .ok_or_else(|| ServerError::team_not_found(format!("no such team `{org}/{team}`")))?;

    if role < AccessRole::OrgAdmin {
        let membership: Option<String> = sqlx::query_scalar(
            "SELECT role FROM team_memberships WHERE team_id = $1 AND user_id = $2",
        )
        .bind(&team_id)
        .bind(&user.id)
        .fetch_optional(&state.db)
        .await
        .map_err(map_sql)?;
        if !is_team_admin_role(membership.as_deref()) {
            return Err(ServerError::team_not_found(format!(
                "no such team `{org}/{team}`"
            )));
        }
    }

    let detail = load_team_detail(&state.db, &org, &team).await?;
    Ok(Json(TeamEnvelope {
        team: detail,
        audit_event_id: None,
    }))
}

async fn add_team_member(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((org, team, email)): Path<(String, String, String)>,
    RegistryJson(body): RegistryJson<TeamRoleBody>,
) -> Result<Json<TeamEnvelope>, ServerError> {
    mutate_team_member(
        &state,
        &user,
        &org,
        &team,
        &email,
        MemberAction::Upsert(body.role),
    )
    .await
}

async fn set_team_member_role(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((org, team, email)): Path<(String, String, String)>,
    RegistryJson(body): RegistryJson<TeamRoleBody>,
) -> Result<Json<TeamEnvelope>, ServerError> {
    mutate_team_member(
        &state,
        &user,
        &org,
        &team,
        &email,
        MemberAction::Update(body.role),
    )
    .await
}

async fn remove_team_member(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((org, team, email)): Path<(String, String, String)>,
) -> Result<Json<TeamEnvelope>, ServerError> {
    mutate_team_member(&state, &user, &org, &team, &email, MemberAction::Remove).await
}

enum MemberAction {
    Upsert(String),
    Update(String),
    Remove,
}

async fn mutate_team_member(
    state: &AppState,
    user: &AuthenticatedUser,
    org: &str,
    team: &str,
    email: &str,
    action: MemberAction,
) -> Result<Json<TeamEnvelope>, ServerError> {
    validate_slug(org).map_err(ServerError::validation_error)?;
    validate_slug(team).map_err(ServerError::validation_error)?;
    require_role(user, org, AccessRole::OrgAdmin).map_err(permission_denied)?;

    let email_norm = email.trim().to_ascii_lowercase();
    let org_id = org_id_by_slug(&state.db, org)
        .await?
        .ok_or_else(|| ServerError::validation_error(format!("unknown org `{org}`")))?;
    let team_id: Option<String> =
        sqlx::query_scalar("SELECT id FROM teams WHERE org_id = $1 AND slug = $2")
            .bind(&org_id)
            .bind(team)
            .fetch_optional(&state.db)
            .await
            .map_err(map_sql)?;
    let team_id = team_id
        .ok_or_else(|| ServerError::team_not_found(format!("no such team `{org}/{team}`")))?;

    let user_id: Option<String> = sqlx::query_scalar("SELECT id FROM users WHERE email = $1")
        .bind(&email_norm)
        .fetch_optional(&state.db)
        .await
        .map_err(map_sql)?;
    let user_id = user_id
        .ok_or_else(|| ServerError::validation_error(format!("unknown user `{email_norm}`")))?;

    let audit_event_id = match action {
        MemberAction::Upsert(role) => {
            let role = parse_team_role(&role)?;
            let mut tx = state.db.begin().await.map_err(map_sql)?;
            sqlx::query("SELECT id FROM teams WHERE id = $1 FOR UPDATE")
                .bind(&team_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(map_sql)?;
            let is_org_member: Option<String> = sqlx::query_scalar(
                "SELECT user_id FROM org_members WHERE org_id = $1 AND user_id = $2",
            )
            .bind(&org_id)
            .bind(&user_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sql)?;
            if is_org_member.is_none() {
                return Err(ServerError::validation_error(format!(
                    "user `{email_norm}` is not a member of org `{org}`"
                )));
            }
            let existing_role: Option<String> = sqlx::query_scalar(
                "SELECT role FROM team_memberships WHERE team_id = $1 AND user_id = $2",
            )
            .bind(&team_id)
            .bind(&user_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sql)?;
            if existing_role.is_none() {
                let count: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM team_memberships WHERE team_id = $1")
                        .bind(&team_id)
                        .fetch_one(&mut *tx)
                        .await
                        .map_err(map_sql)?;
                if count >= state.quotas.max_team_members_per_team {
                    return Err(ServerError::quota_exceeded(format!(
                        "team `{org}/{team}` has reached the member limit of {}",
                        state.quotas.max_team_members_per_team
                    )));
                }
            } else if is_team_admin_role(existing_role.as_deref()) && role != "team_admin" {
                ensure_team_keeps_admin(&mut tx, &team_id, org, team).await?;
            }
            sqlx::query(
                "INSERT INTO team_memberships (team_id, org_id, user_id, role, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, now(), now())
                 ON CONFLICT(team_id, user_id) DO UPDATE SET
                     role = excluded.role,
                     updated_at = excluded.updated_at",
            )
            .bind(&team_id)
            .bind(&org_id)
            .bind(&user_id)
            .bind(role)
            .execute(&mut *tx)
            .await
            .map_err(map_sql)?;
            let action = if existing_role.is_some() {
                "team.member_role_changed"
            } else {
                "team.member_added"
            };
            let audit_event_id = insert_audit_log(
                &mut tx,
                &org_id,
                &user.id,
                "team",
                &team_id,
                action,
                serde_json::json!({
                    "team": team,
                    "target_email": &email_norm,
                    "role": role,
                }),
            )
            .await?;
            tx.commit().await.map_err(map_sql)?;
            audit_event_id
        }
        MemberAction::Update(role) => {
            let role = parse_team_role(&role)?;
            let mut tx = state.db.begin().await.map_err(map_sql)?;
            sqlx::query("SELECT id FROM teams WHERE id = $1 FOR UPDATE")
                .bind(&team_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(map_sql)?;
            let existing_role: Option<String> = sqlx::query_scalar(
                "SELECT role FROM team_memberships WHERE team_id = $1 AND user_id = $2",
            )
            .bind(&team_id)
            .bind(&user_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sql)?;
            let Some(existing_role) = existing_role else {
                return Err(ServerError::team_not_found(format!(
                    "user `{email_norm}` is not a member of team `{org}/{team}`"
                )));
            };
            if is_team_admin_role(Some(existing_role.as_str())) && role != "team_admin" {
                ensure_team_keeps_admin(&mut tx, &team_id, org, team).await?;
            }
            sqlx::query(
                "UPDATE team_memberships
                 SET role = $1, updated_at = now()
                 WHERE team_id = $2 AND user_id = $3",
            )
            .bind(role)
            .bind(&team_id)
            .bind(&user_id)
            .execute(&mut *tx)
            .await
            .map_err(map_sql)?;
            let audit_event_id = insert_audit_log(
                &mut tx,
                &org_id,
                &user.id,
                "team",
                &team_id,
                "team.member_role_changed",
                serde_json::json!({
                    "team": team,
                    "target_email": &email_norm,
                    "previous_role": existing_role,
                    "role": role,
                }),
            )
            .await?;
            tx.commit().await.map_err(map_sql)?;
            audit_event_id
        }
        MemberAction::Remove => {
            let mut tx = state.db.begin().await.map_err(map_sql)?;
            sqlx::query("SELECT id FROM teams WHERE id = $1 FOR UPDATE")
                .bind(&team_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(map_sql)?;
            let existing_role: Option<String> = sqlx::query_scalar(
                "SELECT role FROM team_memberships WHERE team_id = $1 AND user_id = $2",
            )
            .bind(&team_id)
            .bind(&user_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sql)?;
            let Some(existing_role) = existing_role else {
                return Err(ServerError::team_not_found(format!(
                    "user `{email_norm}` is not a member of team `{org}/{team}`"
                )));
            };
            if is_team_admin_role(Some(existing_role.as_str())) {
                ensure_team_keeps_admin(&mut tx, &team_id, org, team).await?;
            }
            sqlx::query("DELETE FROM team_memberships WHERE team_id = $1 AND user_id = $2")
                .bind(&team_id)
                .bind(&user_id)
                .execute(&mut *tx)
                .await
                .map_err(map_sql)?;
            let audit_event_id = insert_audit_log(
                &mut tx,
                &org_id,
                &user.id,
                "team",
                &team_id,
                "team.member_removed",
                serde_json::json!({
                    "team": team,
                    "target_email": &email_norm,
                    "previous_role": existing_role,
                }),
            )
            .await?;
            tx.commit().await.map_err(map_sql)?;
            audit_event_id
        }
    };

    let detail = load_team_detail(&state.db, org, team).await?;
    Ok(Json(TeamEnvelope {
        team: detail,
        audit_event_id: Some(audit_event_id),
    }))
}

async fn ensure_team_keeps_admin(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    team_id: &str,
    org: &str,
    team: &str,
) -> Result<(), ServerError> {
    let admin_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM team_memberships WHERE team_id = $1 AND role IN ('team_admin', 'lead')",
    )
    .bind(team_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_sql)?;
    if admin_count <= 1 {
        return Err(ServerError::validation_error(format!(
            "team `{org}/{team}` must have at least one team_admin"
        )));
    }
    Ok(())
}

async fn load_team_detail(db: &DbPool, org: &str, team: &str) -> Result<TeamDetail, ServerError> {
    let rows = sqlx::query(
        "SELECT users.email AS email, team_memberships.role AS role
         FROM team_memberships
         JOIN teams ON teams.id = team_memberships.team_id
         JOIN orgs ON orgs.id = teams.org_id
         JOIN users ON users.id = team_memberships.user_id
         WHERE orgs.slug = $1 AND teams.slug = $2
         ORDER BY users.email ASC",
    )
    .bind(org)
    .bind(team)
    .fetch_all(db)
    .await
    .map_err(map_sql)?;

    let members = rows
        .into_iter()
        .map(|row| TeamMember {
            email: row.get("email"),
            role: canonical_team_role(row.get::<String, _>("role").as_str()).to_string(),
        })
        .collect();
    Ok(TeamDetail {
        org: org.to_string(),
        slug: team.to_string(),
        members,
    })
}

fn canonical_team_role(role: &str) -> &'static str {
    if is_team_admin_role(Some(role)) {
        "team_admin"
    } else {
        "member"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum VersionPolicy {
    Current,
    Pinned,
}

impl VersionPolicy {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Pinned => "pinned",
        }
    }
}

#[derive(Debug, Serialize)]
struct StackSummary {
    org: String,
    slug: String,
    name: String,
    description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner_email: Option<String>,
    visibility: Visibility,
    #[serde(skip_serializing_if = "Option::is_none")]
    team: Option<String>,
    item_count: i64,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Serialize)]
struct StackItemSummary {
    skill: String,
    version_policy: VersionPolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    pinned_version: Option<String>,
    position: i64,
    added_at: String,
}

#[derive(Debug, Serialize)]
struct StackDetail {
    org: String,
    slug: String,
    name: String,
    description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner_email: Option<String>,
    visibility: Visibility,
    #[serde(skip_serializing_if = "Option::is_none")]
    team: Option<String>,
    created_at: String,
    updated_at: String,
    items: Vec<StackItemSummary>,
}

#[derive(Debug, Serialize)]
struct StackListEnvelope {
    stacks: Vec<StackSummary>,
}

#[derive(Debug, Serialize)]
struct StackEnvelope {
    stack: StackDetail,
    #[serde(skip_serializing_if = "Option::is_none")]
    audit_event_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct StackResolveStack {
    org: String,
    slug: String,
    name: String,
    visibility: Visibility,
    #[serde(skip_serializing_if = "Option::is_none")]
    team: Option<String>,
}

#[derive(Debug, Serialize)]
struct StackDownloadRoute {
    method: &'static str,
    url: String,
}

#[derive(Debug, Serialize)]
struct StackResolvedItem {
    skill: String,
    version_id: String,
    version: String,
    archive_hash: PackageHash,
    download: StackDownloadRoute,
    version_policy: VersionPolicy,
}

#[derive(Debug, Serialize)]
struct StackResolveEnvelope {
    stack: StackResolveStack,
    resolved_at: String,
    manifest_hash: PackageHash,
    items: Vec<StackResolvedItem>,
}

#[derive(Debug, Deserialize)]
struct CreateStackBody {
    slug: String,
    name: String,
    #[serde(default)]
    description: String,
    visibility: Visibility,
    #[serde(default)]
    team: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PatchStackBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    visibility: Option<Visibility>,
    #[serde(default)]
    team: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StackItemBody {
    skill: String,
    #[serde(default)]
    version_policy: Option<VersionPolicy>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    pinned_version: Option<String>,
    #[serde(default)]
    position: Option<i64>,
}

#[derive(Debug, Clone)]
struct StackCore {
    id: String,
    org_id: String,
    org: String,
    slug: String,
    name: String,
    description: String,
    visibility: Visibility,
    team_id: Option<String>,
    team: Option<String>,
    owner_user_id: String,
    owner_email: Option<String>,
    created_at: String,
    updated_at: String,
    team_role: Option<String>,
}

struct SkillForStack {
    id: String,
    visibility: Visibility,
    team_id: Option<String>,
    owner_user_id: Option<String>,
}

async fn create_stack(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(org): Path<String>,
    RegistryJson(body): RegistryJson<CreateStackBody>,
) -> Result<(StatusCode, Json<StackEnvelope>), ServerError> {
    validate_slug(&org).map_err(ServerError::validation_error)?;
    let role = require_role(&user, &org, AccessRole::Reader).map_err(permission_denied)?;
    validate_stack_slug_and_name(&body.slug, &body.name)?;

    let org_id = org_id_by_slug(&state.db, &org)
        .await?
        .ok_or_else(|| ServerError::validation_error(format!("unknown org `{org}`")))?;
    let (team_id, team_role) = resolve_resource_team(
        &state.db,
        &user,
        &org_id,
        &org,
        body.visibility,
        body.team.as_deref(),
        "stack",
    )
    .await?;
    if !can_create_stack_visibility(role, body.visibility, team_role.as_deref()) {
        return Err(ServerError::forbidden("permission denied"));
    }

    let mut tx = state.db.begin().await.map_err(map_sql)?;

    sqlx::query("SELECT id FROM orgs WHERE id = $1 FOR UPDATE")
        .bind(&org_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_sql)?;

    let exists: Option<String> =
        sqlx::query_scalar("SELECT id FROM stacks WHERE org_id = $1 AND slug = $2")
            .bind(&org_id)
            .bind(&body.slug)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sql)?;
    if exists.is_some() {
        return Err(ServerError::validation_error(format!(
            "stack `{}/{}` already exists",
            org, body.slug
        )));
    }

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stacks WHERE org_id = $1")
        .bind(&org_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_sql)?;
    if count >= state.quotas.max_stacks_per_org {
        return Err(ServerError::quota_exceeded(format!(
            "org `{org}` has reached the stack limit of {}",
            state.quotas.max_stacks_per_org
        )));
    }

    let id = random_id("stk");
    sqlx::query(
        "INSERT INTO stacks
            (id, org_id, slug, name, description, visibility, team_id, owner_user_id,
             created_at, updated_at)
         VALUES
            ($1, $2, $3, $4, $5, $6, $7, $8, now(), now())",
    )
    .bind(&id)
    .bind(&org_id)
    .bind(&body.slug)
    .bind(body.name.trim())
    .bind(body.description.trim())
    .bind(body.visibility.as_str())
    .bind(team_id.as_deref())
    .bind(&user.id)
    .execute(&mut *tx)
    .await
    .map_err(map_sql)?;

    let audit_event_id = insert_audit_log(
        &mut tx,
        &org_id,
        &user.id,
        "stack",
        &id,
        "stack.created",
        serde_json::json!({
            "stack": &body.slug,
            "visibility": body.visibility.as_str(),
            "role": role.as_str(),
        }),
    )
    .await?;
    tx.commit().await.map_err(map_sql)?;

    let detail = load_stack_detail_for_user(&state.db, &user, &org, &body.slug, role)
        .await?
        .ok_or_else(ServerError::internal_error)?;
    Ok((
        StatusCode::CREATED,
        Json(StackEnvelope {
            stack: detail,
            audit_event_id: Some(audit_event_id),
        }),
    ))
}

async fn list_stacks(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(org): Path<String>,
    RegistryQuery(params): RegistryQuery<Vec<(String, String)>>,
) -> Result<Json<StackListEnvelope>, ServerError> {
    let query = ListQuery::from_pairs(params);
    let limit = parse_limit(query.limit.as_deref())?;
    let owner = owner_filter(query.owner.as_deref());
    let team = query
        .team
        .as_deref()
        .map(str::trim)
        .map(|team| {
            validate_slug(team).map_err(ServerError::validation_error)?;
            Ok::<_, ServerError>(team)
        })
        .transpose()?;
    validate_slug(&org).map_err(ServerError::validation_error)?;
    let role = require_role(&user, &org, AccessRole::Reader).map_err(permission_denied)?;

    // A SQL LIMIT is only safe when the loop below skips no rows: org admins
    // pass every visibility check, and the owner/team filters drop rows after
    // the fetch. LIMIT NULL means no limit in Postgres.
    let sql_limit = if owner.is_none() && team.is_none() && role >= AccessRole::OrgAdmin {
        limit.map(|limit| limit as i64)
    } else {
        None
    };
    let rows = sqlx::query(
        "SELECT orgs.slug AS org_slug, stacks.slug, stacks.name, stacks.description,
                stacks.visibility, teams.slug AS team_slug, stacks.owner_user_id,
                owner_users.email AS owner_email,
                to_char(stacks.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at,
                to_char(stacks.updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS updated_at,
                COUNT(stack_items.id) AS item_count,
                stack_team_memberships.role AS team_role
         FROM stacks
         JOIN orgs ON orgs.id = stacks.org_id
         JOIN users AS owner_users ON owner_users.id = stacks.owner_user_id
         LEFT JOIN teams ON teams.id = stacks.team_id
         LEFT JOIN stack_items ON stack_items.stack_id = stacks.id
         LEFT JOIN team_memberships AS stack_team_memberships
           ON stack_team_memberships.team_id = stacks.team_id
          AND stack_team_memberships.user_id = $1
         WHERE orgs.slug = $2
         GROUP BY orgs.slug, stacks.id, stacks.slug, stacks.name, stacks.description,
                  stacks.visibility, teams.slug, stacks.owner_user_id, owner_users.email,
                  stacks.created_at, stacks.updated_at, stack_team_memberships.role
         ORDER BY stacks.slug ASC
         LIMIT $3",
    )
    .bind(&user.id)
    .bind(&org)
    .bind(sql_limit)
    .fetch_all(&state.db)
    .await
    .map_err(map_sql)?;

    let mut stacks = Vec::new();
    for row in rows {
        let visibility = visibility_from_db(row.get::<String, _>("visibility").as_str())?;
        let owner_user_id: String = row.get("owner_user_id");
        let team_role: Option<String> = row.get("team_role");
        if !can_read_visibility(
            &user,
            role,
            visibility,
            Some(owner_user_id.as_str()),
            team_role.is_some(),
        ) {
            continue;
        }
        let owner_email: Option<String> = row.get("owner_email");
        if let Some(filter) = owner.as_deref()
            && owner_email
                .as_deref()
                .map(str::to_ascii_lowercase)
                .as_deref()
                != Some(filter)
        {
            continue;
        }
        let team_slug: Option<String> = row.get("team_slug");
        if let Some(filter) = team
            && team_slug.as_deref() != Some(filter)
        {
            continue;
        }
        stacks.push(StackSummary {
            org: row.get("org_slug"),
            slug: row.get("slug"),
            name: row.get("name"),
            description: row.get("description"),
            owner_email,
            visibility,
            team: team_slug,
            item_count: row.get("item_count"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        });
        if limit.is_some_and(|limit| stacks.len() >= limit) {
            break;
        }
    }

    Ok(Json(StackListEnvelope { stacks }))
}

async fn get_stack(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((org, stack)): Path<(String, String)>,
) -> Result<Json<StackEnvelope>, ServerError> {
    validate_slug(&org).map_err(ServerError::validation_error)?;
    validate_slug(&stack).map_err(ServerError::validation_error)?;
    let role = require_role(&user, &org, AccessRole::Reader).map_err(permission_denied)?;

    let stack = load_stack_detail_for_user(&state.db, &user, &org, &stack, role)
        .await?
        .ok_or_else(|| stack_not_found(&org, &stack))?;
    Ok(Json(StackEnvelope {
        stack,
        audit_event_id: None,
    }))
}

async fn stack_status(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((org, stack)): Path<(String, String)>,
) -> Result<Json<StackEnvelope>, ServerError> {
    get_stack(State(state), user, Path((org, stack))).await
}

async fn stack_audit(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((org, stack)): Path<(String, String)>,
) -> Result<Json<AuditListEnvelope>, ServerError> {
    validate_slug(&org).map_err(ServerError::validation_error)?;
    validate_slug(&stack).map_err(ServerError::validation_error)?;
    let role = require_role(&user, &org, AccessRole::Reader).map_err(permission_denied)?;
    let stack_core = load_stack_core_for_user(&state.db, &user, &org, &stack, role)
        .await?
        .ok_or_else(|| stack_not_found(&org, &stack))?;
    if role < AccessRole::OrgAdmin && !can_manage_stack(&user, role, &stack_core) {
        return Err(ServerError::forbidden("permission denied"));
    }
    let events = list_audit_events(
        &state.db,
        &org,
        Some(("stack", stack_core.id.as_str())),
        None,
        Some(MAX_HTTP_AUDIT_EVENTS),
    )
    .await
    .map_err(map_sql)?;
    Ok(Json(AuditListEnvelope { events }))
}

async fn patch_stack(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((org, stack)): Path<(String, String)>,
    RegistryJson(body): RegistryJson<PatchStackBody>,
) -> Result<Json<StackEnvelope>, ServerError> {
    validate_slug(&org).map_err(ServerError::validation_error)?;
    validate_slug(&stack).map_err(ServerError::validation_error)?;
    let role = require_role(&user, &org, AccessRole::Reader).map_err(permission_denied)?;
    let current = load_stack_core_for_user(&state.db, &user, &org, &stack, role)
        .await?
        .ok_or_else(|| stack_not_found(&org, &stack))?;
    if !can_manage_stack(&user, role, &current) {
        return Err(ServerError::forbidden("permission denied"));
    }

    let mut tx = state.db.begin().await.map_err(map_sql)?;
    let locked = lock_stack_core_for_update(&mut tx, &user, &org, &stack, role)
        .await?
        .ok_or_else(|| stack_not_found(&org, &stack))?;
    if !can_manage_stack(&user, role, &locked) {
        return Err(ServerError::forbidden("permission denied"));
    }

    let name = body
        .name
        .as_deref()
        .map(str::trim)
        .unwrap_or(locked.name.as_str());
    let description = body
        .description
        .as_deref()
        .map(str::trim)
        .unwrap_or(locked.description.as_str());
    let visibility = body.visibility.unwrap_or(locked.visibility);
    if name.is_empty() {
        return Err(ServerError::validation_error(
            "stack.name must not be empty",
        ));
    }

    let team_slug = if visibility == Visibility::Team {
        body.team.as_deref().or(locked.team.as_deref())
    } else {
        if body.team.is_some() {
            return Err(ServerError::validation_error(
                "stack.team is only valid with visibility `team`",
            ));
        }
        None
    };
    let visibility_changed = visibility != locked.visibility || team_slug != locked.team.as_deref();
    if visibility_changed && role < AccessRole::OrgAdmin {
        return Err(ServerError::forbidden("permission denied"));
    }
    let (team_id, team_role) = resolve_resource_team(
        &state.db,
        &user,
        &locked.org_id,
        &org,
        visibility,
        team_slug,
        "stack",
    )
    .await?;
    if !can_publish_visibility(role, visibility, team_role.as_deref()) {
        return Err(forbidden_publish(role));
    }
    validate_existing_stack_items(
        &mut tx,
        &locked.id,
        &locked.org_id,
        visibility,
        team_id.as_deref(),
        &locked.owner_user_id,
    )
    .await?;

    sqlx::query(
        "UPDATE stacks
         SET name = $1,
             description = $2,
             visibility = $3,
             team_id = $4,
             updated_at = now()
         WHERE id = $5",
    )
    .bind(name)
    .bind(description)
    .bind(visibility.as_str())
    .bind(team_id.as_deref())
    .bind(&locked.id)
    .execute(&mut *tx)
    .await
    .map_err(map_sql)?;

    let audit_event_id = insert_audit_log(
        &mut tx,
        &locked.org_id,
        &user.id,
        "stack",
        &locked.id,
        "stack.updated",
        serde_json::json!({
            "stack": &stack,
            "previous_visibility": locked.visibility.as_str(),
            "visibility": visibility.as_str(),
            "role": role.as_str(),
        }),
    )
    .await?;
    tx.commit().await.map_err(map_sql)?;

    let detail = load_stack_detail_for_user(&state.db, &user, &org, &stack, role)
        .await?
        .ok_or_else(ServerError::internal_error)?;
    Ok(Json(StackEnvelope {
        stack: detail,
        audit_event_id: Some(audit_event_id),
    }))
}

async fn patch_stack_visibility(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((org, stack)): Path<(String, String)>,
    RegistryJson(body): RegistryJson<VisibilityPatchBody>,
) -> Result<Json<StackEnvelope>, ServerError> {
    if body.visibility == Visibility::Team && body.team.is_none() {
        return Err(ServerError::validation_error(
            "stack.team is required with visibility `team`",
        ));
    }
    patch_stack(
        State(state),
        user,
        Path((org, stack)),
        RegistryJson(PatchStackBody {
            name: None,
            description: None,
            visibility: Some(body.visibility),
            team: body.team,
        }),
    )
    .await
}

async fn upsert_stack_item(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((org, stack)): Path<(String, String)>,
    RegistryJson(body): RegistryJson<StackItemBody>,
) -> Result<Json<StackEnvelope>, ServerError> {
    validate_slug(&org).map_err(ServerError::validation_error)?;
    validate_slug(&stack).map_err(ServerError::validation_error)?;
    validate_slug(&body.skill).map_err(ServerError::validation_error)?;
    let role = require_role(&user, &org, AccessRole::Reader).map_err(permission_denied)?;
    let stack_core = load_stack_core_for_user(&state.db, &user, &org, &stack, role)
        .await?
        .ok_or_else(|| stack_not_found(&org, &stack))?;
    if !can_manage_stack(&user, role, &stack_core) {
        return Err(ServerError::forbidden("permission denied"));
    }

    let version_policy = body.version_policy.unwrap_or(VersionPolicy::Current);
    let requested_position = match body.position {
        Some(position) if position >= 0 => Some(position + 1),
        Some(_) => {
            return Err(ServerError::validation_error(
                "stack item position must be zero or greater",
            ));
        }
        None => None,
    };

    let pinned_version = match version_policy {
        VersionPolicy::Current => {
            if body.version.is_some() || body.pinned_version.is_some() {
                return Err(ServerError::validation_error(
                    "version is only valid when version_policy is `pinned`",
                ));
            }
            None
        }
        VersionPolicy::Pinned => Some(
            body.pinned_version
                .as_deref()
                .or(body.version.as_deref())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    ServerError::validation_error(
                        "version is required when version_policy is `pinned`",
                    )
                })?
                .to_string(),
        ),
    };

    let mut tx = state.db.begin().await.map_err(map_sql)?;
    let locked_stack = lock_stack_core_for_update(&mut tx, &user, &org, &stack, role)
        .await?
        .ok_or_else(|| stack_not_found(&org, &stack))?;
    if !can_manage_stack(&user, role, &locked_stack) {
        return Err(ServerError::forbidden("permission denied"));
    }
    let skill = load_skill_for_stack_item(&mut tx, &user, role, &locked_stack, &body.skill).await?;
    let pinned_version_id = match pinned_version.as_deref() {
        Some(version) => {
            Some(load_pinned_version_id(&mut tx, &skill.id, &org, &body.skill, version).await?)
        }
        None => None,
    };
    validate_stack_can_include_skill(&mut tx, &locked_stack, &skill).await?;

    let db_position = match requested_position {
        Some(position) => position,
        None => next_stack_item_position_tx(&mut tx, &locked_stack.id).await?,
    };

    let existing_item: Option<String> =
        sqlx::query_scalar("SELECT id FROM stack_items WHERE stack_id = $1 AND skill_id = $2")
            .bind(&locked_stack.id)
            .bind(&skill.id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sql)?;
    if existing_item.is_none() {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stack_items WHERE stack_id = $1")
            .bind(&locked_stack.id)
            .fetch_one(&mut *tx)
            .await
            .map_err(map_sql)?;
        if count >= state.quotas.max_stack_items_per_stack {
            return Err(ServerError::quota_exceeded(format!(
                "stack `{org}/{stack}` has reached the item limit of {}",
                state.quotas.max_stack_items_per_stack
            )));
        }
    }

    sqlx::query(
        "INSERT INTO stack_items
            (id, stack_id, skill_id, version_policy, pinned_version_id, position,
             added_by_user_id, added_at)
         VALUES
            ($1, $2, $3, $4, $5, $6, $7, now())
         ON CONFLICT(stack_id, skill_id) DO UPDATE SET
             version_policy = excluded.version_policy,
             pinned_version_id = excluded.pinned_version_id,
             position = excluded.position,
             added_by_user_id = excluded.added_by_user_id",
    )
    .bind(random_id("sti"))
    .bind(&locked_stack.id)
    .bind(&skill.id)
    .bind(version_policy.as_str())
    .bind(pinned_version_id.as_deref())
    .bind(db_position)
    .bind(&user.id)
    .execute(&mut *tx)
    .await
    .map_err(map_sql)?;
    touch_stack_tx(&mut tx, &locked_stack.id).await?;

    let audit_event_id = insert_audit_log(
        &mut tx,
        &locked_stack.org_id,
        &user.id,
        "stack",
        &locked_stack.id,
        "stack.item_upserted",
        serde_json::json!({
            "stack": &stack,
            "skill": body.skill,
            "version_policy": version_policy.as_str(),
            "pinned_version": body.pinned_version.or(body.version),
            "role": role.as_str(),
        }),
    )
    .await?;
    tx.commit().await.map_err(map_sql)?;

    let detail = load_stack_detail_for_user(&state.db, &user, &org, &stack, role)
        .await?
        .ok_or_else(ServerError::internal_error)?;
    Ok(Json(StackEnvelope {
        stack: detail,
        audit_event_id: Some(audit_event_id),
    }))
}

async fn delete_stack_item(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((org, stack, skill)): Path<(String, String, String)>,
) -> Result<Json<StackEnvelope>, ServerError> {
    validate_slug(&org).map_err(ServerError::validation_error)?;
    validate_slug(&stack).map_err(ServerError::validation_error)?;
    validate_slug(&skill).map_err(ServerError::validation_error)?;
    let role = require_role(&user, &org, AccessRole::Reader).map_err(permission_denied)?;
    let stack_core = load_stack_core_for_user(&state.db, &user, &org, &stack, role)
        .await?
        .ok_or_else(|| stack_not_found(&org, &stack))?;
    if !can_manage_stack(&user, role, &stack_core) {
        return Err(ServerError::forbidden("permission denied"));
    }

    let mut tx = state.db.begin().await.map_err(map_sql)?;

    let result = sqlx::query(
        "DELETE FROM stack_items
         WHERE stack_id = $1
           AND skill_id = (
             SELECT id FROM skills WHERE org_id = $2 AND name = $3
           )",
    )
    .bind(&stack_core.id)
    .bind(&stack_core.org_id)
    .bind(&skill)
    .execute(&mut *tx)
    .await
    .map_err(map_sql)?;
    if result.rows_affected() == 0 {
        return Err(ServerError::skill_not_found(format!(
            "skill `{org}/{skill}` is not in stack `{org}/{stack}`"
        )));
    }
    touch_stack_tx(&mut tx, &stack_core.id).await?;

    let audit_event_id = insert_audit_log(
        &mut tx,
        &stack_core.org_id,
        &user.id,
        "stack",
        &stack_core.id,
        "stack.item_removed",
        serde_json::json!({
            "stack": &stack,
            "skill": &skill,
            "role": role.as_str(),
        }),
    )
    .await?;
    tx.commit().await.map_err(map_sql)?;

    let detail = load_stack_detail_for_user(&state.db, &user, &org, &stack, role)
        .await?
        .ok_or_else(ServerError::internal_error)?;
    Ok(Json(StackEnvelope {
        stack: detail,
        audit_event_id: Some(audit_event_id),
    }))
}

async fn resolve_stack(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((org, stack)): Path<(String, String)>,
) -> Result<Json<StackResolveEnvelope>, ServerError> {
    validate_slug(&org).map_err(ServerError::validation_error)?;
    validate_slug(&stack).map_err(ServerError::validation_error)?;
    let role = require_role(&user, &org, AccessRole::Reader).map_err(permission_denied)?;
    let stack_core = load_stack_core_for_user(&state.db, &user, &org, &stack, role)
        .await?
        .ok_or_else(|| stack_not_found(&org, &stack))?;

    let rows = sqlx::query(
        "SELECT stack_items.version_policy,
                skills.name AS skill_name, skills.visibility AS skill_visibility,
                teams.slug AS skill_team_slug,
                skills.owner_user_id AS skill_owner_user_id,
                (SELECT team_memberships.role
                 FROM team_memberships
                 WHERE team_memberships.team_id = skills.team_id
                   AND team_memberships.user_id = $1) AS skill_team_role,
                selected_versions.id AS version_id,
                selected_versions.version_number::text AS version,
                selected_versions.status AS status,
                to_char(selected_versions.yanked_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS yanked_at,
                archives.hash_algorithm, archives.hash_hex
         FROM stack_items
         JOIN skills ON skills.id = stack_items.skill_id
         LEFT JOIN teams ON teams.id = skills.team_id
         LEFT JOIN skill_versions AS selected_versions
           ON selected_versions.id = CASE
                WHEN stack_items.version_policy = 'current' THEN skills.current_version_id
                ELSE stack_items.pinned_version_id
              END
         LEFT JOIN archives ON archives.id = selected_versions.archive_id
         WHERE stack_items.stack_id = $2
         ORDER BY stack_items.position ASC, stack_items.added_at ASC, skills.name ASC",
    )
    .bind(&user.id)
    .bind(&stack_core.id)
    .fetch_all(&state.db)
    .await
    .map_err(map_sql)?;

    let mut items = Vec::new();
    for row in rows {
        let skill_visibility =
            visibility_from_db(row.get::<String, _>("skill_visibility").as_str())?;
        let owner_user_id: Option<String> = row.get("skill_owner_user_id");
        let team_role: Option<String> = row.get("skill_team_role");
        if !can_read_visibility(
            &user,
            role,
            skill_visibility,
            owner_user_id.as_deref(),
            team_role.is_some(),
        ) {
            return Err(stack_resolution_failed());
        }

        let version_id: Option<String> = row.get("version_id");
        let version: Option<String> = row.get("version");
        let status: Option<String> = row.get("status");
        let yanked_at: Option<String> = row.get("yanked_at");
        let hash_algorithm: Option<String> = row.get("hash_algorithm");
        let hash_hex: Option<String> = row.get("hash_hex");
        let (Some(version_id), Some(version), Some(status), Some(hash_algorithm), Some(hash_hex)) =
            (version_id, version, status, hash_algorithm, hash_hex)
        else {
            return Err(stack_resolution_failed());
        };
        if status != "approved" || yanked_at.is_some() || hash_algorithm != "sha256" {
            return Err(stack_resolution_failed());
        }

        let skill: String = row.get("skill_name");
        let version_policy =
            version_policy_from_db(row.get::<String, _>("version_policy").as_str())?;
        items.push(StackResolvedItem {
            skill: skill.clone(),
            version_id,
            version: version.clone(),
            archive_hash: PackageHash {
                algorithm: hash_algorithm,
                hex: hash_hex,
            },
            download: StackDownloadRoute {
                method: "GET",
                url: format!("/v1/orgs/{org}/skills/{skill}/versions/{version}/archive"),
            },
            version_policy,
        });
    }

    let stack_header = StackResolveStack {
        org: stack_core.org,
        slug: stack_core.slug,
        name: stack_core.name,
        visibility: stack_core.visibility,
        team: stack_core.team,
    };
    let manifest_body = serde_json::json!({
        "stack": &stack_header,
        "items": &items,
    });
    let manifest_bytes =
        serde_json::to_vec(&manifest_body).map_err(|_| ServerError::internal_error())?;
    let resolved_at: String = sqlx::query_scalar(
        "SELECT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')",
    )
    .fetch_one(&state.db)
    .await
    .map_err(map_sql)?;

    Ok(Json(StackResolveEnvelope {
        stack: stack_header,
        resolved_at,
        manifest_hash: PackageHash {
            algorithm: "sha256".to_string(),
            hex: sha256_hex(&manifest_bytes),
        },
        items,
    }))
}

async fn org_audit(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(org): Path<String>,
) -> Result<Json<AuditListEnvelope>, ServerError> {
    validate_slug(&org).map_err(ServerError::validation_error)?;
    require_role(&user, &org, AccessRole::OrgAdmin).map_err(permission_denied)?;
    let events = list_audit_events(&state.db, &org, None, None, Some(MAX_HTTP_AUDIT_EVENTS))
        .await
        .map_err(map_sql)?;
    Ok(Json(AuditListEnvelope { events }))
}

async fn org_audit_event(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((org, event_id)): Path<(String, String)>,
) -> Result<Json<AuditEventEnvelope>, ServerError> {
    validate_slug(&org).map_err(ServerError::validation_error)?;
    require_role(&user, &org, AccessRole::OrgAdmin).map_err(permission_denied)?;
    let mut events = list_audit_events(&state.db, &org, None, Some(event_id.as_str()), Some(1))
        .await
        .map_err(map_sql)?;
    let event = events.pop().ok_or_else(|| {
        ServerError::audit_event_not_found(format!("no such audit event `{event_id}`"))
    })?;
    Ok(Json(AuditEventEnvelope { event }))
}

fn validate_stack_slug_and_name(slug: &str, name: &str) -> Result<(), ServerError> {
    validate_slug(slug).map_err(ServerError::validation_error)?;
    if name.trim().is_empty() {
        return Err(ServerError::validation_error(
            "stack.name must not be empty",
        ));
    }
    Ok(())
}

/// Resolve the `(team_id, caller's team_role)` for a team-scoped skill or stack and
/// enforce the visibility/team coupling. `resource` ("skill"/"stack") only shapes the
/// validation messages.
async fn resolve_resource_team(
    db: &DbPool,
    user: &AuthenticatedUser,
    org_id: &str,
    org: &str,
    visibility: Visibility,
    team: Option<&str>,
    resource: &str,
) -> Result<(Option<String>, Option<String>), ServerError> {
    match visibility {
        Visibility::Private | Visibility::Org => {
            if team.is_some() {
                return Err(ServerError::validation_error(format!(
                    "{resource}.team is only valid with visibility `team`"
                )));
            }
            Ok((None, None))
        }
        Visibility::Team => {
            let team = team.ok_or_else(|| {
                ServerError::validation_error(format!(
                    "{resource}.team is required with visibility `team`"
                ))
            })?;
            validate_slug(team).map_err(ServerError::validation_error)?;
            let row = sqlx::query(
                "SELECT teams.id AS team_id,
                        (SELECT team_memberships.role
                         FROM team_memberships
                         WHERE team_memberships.team_id = teams.id
                           AND team_memberships.user_id = $1) AS team_role
                 FROM teams
                 WHERE teams.org_id = $2 AND teams.slug = $3",
            )
            .bind(&user.id)
            .bind(org_id)
            .bind(team)
            .fetch_optional(db)
            .await
            .map_err(map_sql)?
            .ok_or_else(|| ServerError::validation_error(format!("unknown team `{org}/{team}`")))?;
            Ok((Some(row.get("team_id")), row.get("team_role")))
        }
    }
}

async fn load_stack_detail_for_user(
    db: &DbPool,
    user: &AuthenticatedUser,
    org: &str,
    stack: &str,
    role: AccessRole,
) -> Result<Option<StackDetail>, ServerError> {
    let Some(core) = load_stack_core_for_user(db, user, org, stack, role).await? else {
        return Ok(None);
    };
    let rows = sqlx::query(
        "SELECT skills.name AS skill_name, stack_items.version_policy,
                pinned_versions.version_number::text AS pinned_version,
                stack_items.position - 1 AS position,
                to_char(stack_items.added_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS added_at,
                skills.visibility AS skill_visibility,
                skills.owner_user_id AS skill_owner_user_id,
                (SELECT team_memberships.role
                 FROM team_memberships
                 WHERE team_memberships.team_id = skills.team_id
                   AND team_memberships.user_id = $1) AS skill_team_role
         FROM stack_items
         JOIN skills ON skills.id = stack_items.skill_id
         LEFT JOIN skill_versions AS pinned_versions
           ON pinned_versions.id = stack_items.pinned_version_id
         WHERE stack_items.stack_id = $2
         ORDER BY stack_items.position ASC, stack_items.added_at ASC, skills.name ASC",
    )
    .bind(&user.id)
    .bind(&core.id)
    .fetch_all(db)
    .await
    .map_err(map_sql)?;

    let mut items = Vec::new();
    for row in rows {
        let skill_visibility =
            visibility_from_db(row.get::<String, _>("skill_visibility").as_str())?;
        let owner_user_id: Option<String> = row.get("skill_owner_user_id");
        let team_role: Option<String> = row.get("skill_team_role");
        if !can_read_visibility(
            user,
            role,
            skill_visibility,
            owner_user_id.as_deref(),
            team_role.is_some(),
        ) {
            continue;
        }
        items.push(StackItemSummary {
            skill: row.get("skill_name"),
            version_policy: version_policy_from_db(
                row.get::<String, _>("version_policy").as_str(),
            )?,
            pinned_version: row.get("pinned_version"),
            position: row.get("position"),
            added_at: row.get("added_at"),
        });
    }

    Ok(Some(StackDetail {
        org: core.org,
        slug: core.slug,
        name: core.name,
        description: core.description,
        owner_email: core.owner_email,
        visibility: core.visibility,
        team: core.team,
        created_at: core.created_at,
        updated_at: core.updated_at,
        items,
    }))
}

/// Binds: $1 user id (for team_role), $2 org slug, $3 stack slug.
const STACK_CORE_SQL: &str =
    "SELECT stacks.id, stacks.org_id, orgs.slug AS org_slug, stacks.slug,
            stacks.name, stacks.description, stacks.visibility,
            stacks.team_id, teams.slug AS team_slug, stacks.owner_user_id,
            owner_users.email AS owner_email,
            to_char(stacks.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at,
            to_char(stacks.updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS updated_at,
            (SELECT team_memberships.role
             FROM team_memberships
             WHERE team_memberships.team_id = stacks.team_id
               AND team_memberships.user_id = $1) AS team_role
     FROM stacks
     JOIN orgs ON orgs.id = stacks.org_id
     JOIN users AS owner_users ON owner_users.id = stacks.owner_user_id
     LEFT JOIN teams ON teams.id = stacks.team_id
     WHERE orgs.slug = $2 AND stacks.slug = $3";

/// Convert a `STACK_CORE_SQL` row into a `StackCore`, returning `None` when the
/// caller is not allowed to see the stack.
fn visible_stack_core(
    row: Option<crate::db::DbRow>,
    user: &AuthenticatedUser,
    role: AccessRole,
) -> Result<Option<StackCore>, ServerError> {
    let Some(row) = row else {
        return Ok(None);
    };
    let core = stack_core_from_row(&row)?;
    if !can_read_visibility(
        user,
        role,
        core.visibility,
        Some(core.owner_user_id.as_str()),
        core.team_role.is_some(),
    ) {
        return Ok(None);
    }
    Ok(Some(core))
}

async fn load_stack_core_for_user(
    db: &DbPool,
    user: &AuthenticatedUser,
    org: &str,
    stack: &str,
    role: AccessRole,
) -> Result<Option<StackCore>, ServerError> {
    let row = sqlx::query(STACK_CORE_SQL)
        .bind(&user.id)
        .bind(org)
        .bind(stack)
        .fetch_optional(db)
        .await
        .map_err(map_sql)?;
    visible_stack_core(row, user, role)
}

async fn lock_stack_core_for_update(
    db: &mut crate::db::DbTransaction<'_>,
    user: &AuthenticatedUser,
    org: &str,
    stack: &str,
    role: AccessRole,
) -> Result<Option<StackCore>, ServerError> {
    let row = sqlx::query(&format!("{STACK_CORE_SQL} FOR UPDATE OF stacks"))
        .bind(&user.id)
        .bind(org)
        .bind(stack)
        .fetch_optional(&mut **db)
        .await
        .map_err(map_sql)?;
    visible_stack_core(row, user, role)
}

fn stack_core_from_row(row: &crate::db::DbRow) -> Result<StackCore, ServerError> {
    Ok(StackCore {
        id: row.get("id"),
        org_id: row.get("org_id"),
        org: row.get("org_slug"),
        slug: row.get("slug"),
        name: row.get("name"),
        description: row.get("description"),
        visibility: visibility_from_db(row.get::<String, _>("visibility").as_str())?,
        team_id: row.get("team_id"),
        team: row.get("team_slug"),
        owner_user_id: row.get("owner_user_id"),
        owner_email: row.get("owner_email"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        team_role: row.get("team_role"),
    })
}

fn can_manage_stack(user: &AuthenticatedUser, role: AccessRole, stack: &StackCore) -> bool {
    if role >= AccessRole::OrgAdmin {
        return true;
    }
    match stack.visibility {
        Visibility::Org => false,
        Visibility::Private => stack.owner_user_id == user.id && role >= AccessRole::Publisher,
        Visibility::Team => {
            is_team_admin_role(stack.team_role.as_deref())
                || (stack.owner_user_id == user.id && role >= AccessRole::Publisher)
        }
    }
}

fn can_create_stack_visibility(
    role: AccessRole,
    visibility: Visibility,
    team_role: Option<&str>,
) -> bool {
    match visibility {
        Visibility::Private => role >= AccessRole::Publisher,
        Visibility::Org => role >= AccessRole::OrgAdmin,
        Visibility::Team => role >= AccessRole::OrgAdmin || is_team_admin_role(team_role),
    }
}

async fn load_skill_for_stack_item(
    db: &mut crate::db::DbTransaction<'_>,
    user: &AuthenticatedUser,
    role: AccessRole,
    stack: &StackCore,
    skill: &str,
) -> Result<SkillForStack, ServerError> {
    let row = sqlx::query(
        "SELECT skills.id, skills.visibility, skills.team_id,
                skills.owner_user_id AS owner_user_id,
                (SELECT team_memberships.role
                 FROM team_memberships
                 WHERE team_memberships.team_id = skills.team_id
                   AND team_memberships.user_id = $1) AS team_role
         FROM skills
         WHERE skills.org_id = $2 AND skills.name = $3
         FOR UPDATE OF skills",
    )
    .bind(&user.id)
    .bind(&stack.org_id)
    .bind(skill)
    .fetch_optional(&mut **db)
    .await
    .map_err(map_sql)?
    .ok_or_else(|| {
        ServerError::skill_not_found(format!("no such skill `{}/{skill}`", stack.org))
    })?;

    let visibility = visibility_from_db(row.get::<String, _>("visibility").as_str())?;
    let owner_user_id: Option<String> = row.get("owner_user_id");
    let team_role: Option<String> = row.get("team_role");
    if !can_read_visibility(
        user,
        role,
        visibility,
        owner_user_id.as_deref(),
        team_role.is_some(),
    ) {
        return Err(ServerError::skill_not_found(format!(
            "no such skill `{}/{skill}`",
            stack.org
        )));
    }

    Ok(SkillForStack {
        id: row.get("id"),
        visibility,
        team_id: row.get("team_id"),
        owner_user_id,
    })
}

async fn validate_stack_can_include_skill(
    db: &mut crate::db::DbTransaction<'_>,
    stack: &StackCore,
    skill: &SkillForStack,
) -> Result<(), ServerError> {
    match stack.visibility {
        Visibility::Org => {
            if skill.visibility != Visibility::Org {
                return Err(ServerError::validation_error(
                    "org stacks can include org-visible skills only",
                ));
            }
        }
        Visibility::Team => match skill.visibility {
            Visibility::Org => {}
            Visibility::Private => {
                return Err(ServerError::validation_error(
                    "non-private stacks cannot include private skills",
                ));
            }
            Visibility::Team if skill.team_id == stack.team_id => {}
            Visibility::Team => {
                return Err(ServerError::validation_error(
                    "team stacks cannot include another team's team-visible skills",
                ));
            }
        },
        Visibility::Private => {
            if !stack_owner_can_read_skill(db, stack, skill).await? {
                return Err(ServerError::validation_error(
                    "private stacks can only include skills visible to the stack owner",
                ));
            }
        }
    }
    Ok(())
}

async fn validate_existing_stack_items(
    db: &mut crate::db::DbTransaction<'_>,
    stack_id: &str,
    org_id: &str,
    visibility: Visibility,
    team_id: Option<&str>,
    owner_user_id: &str,
) -> Result<(), ServerError> {
    let rows = sqlx::query(
        "SELECT skills.id, skills.visibility, skills.team_id,
                skills.owner_user_id AS owner_user_id,
                stack_owner.is_server_admin AS stack_owner_is_server_admin,
                stack_owner_org_members.role AS stack_owner_org_role,
                stack_owner_team_memberships.role AS stack_owner_team_role
         FROM stack_items
         JOIN skills ON skills.id = stack_items.skill_id
         LEFT JOIN users AS stack_owner ON stack_owner.id = $2
         LEFT JOIN org_members AS stack_owner_org_members
           ON stack_owner_org_members.user_id = stack_owner.id
          AND stack_owner_org_members.org_id = $3
         LEFT JOIN team_memberships AS stack_owner_team_memberships
           ON stack_owner_team_memberships.team_id = skills.team_id
          AND stack_owner_team_memberships.user_id = stack_owner.id
         WHERE stack_items.stack_id = $1",
    )
    .bind(stack_id)
    .bind(owner_user_id)
    .bind(org_id)
    .fetch_all(&mut **db)
    .await
    .map_err(map_sql)?;

    let check_stack = StackCore {
        id: stack_id.to_string(),
        org_id: org_id.to_string(),
        org: String::new(),
        slug: String::new(),
        name: String::new(),
        description: String::new(),
        visibility,
        team_id: team_id.map(str::to_string),
        team: None,
        owner_user_id: owner_user_id.to_string(),
        owner_email: None,
        created_at: String::new(),
        updated_at: String::new(),
        team_role: None,
    };
    for row in rows {
        let skill = SkillForStack {
            id: row.get("id"),
            visibility: visibility_from_db(row.get::<String, _>("visibility").as_str())?,
            team_id: row.get("team_id"),
            owner_user_id: row.get("owner_user_id"),
        };
        validate_stack_can_include_skill_from_owner_row(&check_stack, &skill, &row)?;
    }
    Ok(())
}

fn validate_stack_can_include_skill_from_owner_row(
    stack: &StackCore,
    skill: &SkillForStack,
    row: &crate::db::DbRow,
) -> Result<(), ServerError> {
    match stack.visibility {
        Visibility::Org => {
            if skill.visibility != Visibility::Org {
                return Err(ServerError::validation_error(
                    "org stacks can include org-visible skills only",
                ));
            }
        }
        Visibility::Team => match skill.visibility {
            Visibility::Org => {}
            Visibility::Private => {
                return Err(ServerError::validation_error(
                    "non-private stacks cannot include private skills",
                ));
            }
            Visibility::Team if skill.team_id == stack.team_id => {}
            Visibility::Team => {
                return Err(ServerError::validation_error(
                    "team stacks cannot include another team's team-visible skills",
                ));
            }
        },
        Visibility::Private => {
            let owner_role = stack_owner_access_role(
                row.get("stack_owner_is_server_admin"),
                row.get::<Option<String>, _>("stack_owner_org_role")
                    .as_deref(),
            );
            let team_role: Option<String> = row.get("stack_owner_team_role");
            if !stack_owner_can_read_skill_with_role(
                &stack.owner_user_id,
                owner_role,
                team_role.as_deref(),
                skill,
            ) {
                return Err(ServerError::validation_error(
                    "private stacks can only include skills visible to the stack owner",
                ));
            }
        }
    }
    Ok(())
}

async fn stack_owner_can_read_skill(
    db: &mut crate::db::DbTransaction<'_>,
    stack: &StackCore,
    skill: &SkillForStack,
) -> Result<bool, ServerError> {
    let Some(row) = sqlx::query(
        "SELECT users.is_server_admin, org_members.role AS org_role,
                (SELECT team_memberships.role
                 FROM team_memberships
                 WHERE team_memberships.team_id = $1
                   AND team_memberships.user_id = users.id) AS team_role
         FROM users
         LEFT JOIN org_members
           ON org_members.user_id = users.id
          AND org_members.org_id = $2
         WHERE users.id = $3",
    )
    .bind(skill.team_id.as_deref())
    .bind(&stack.org_id)
    .bind(&stack.owner_user_id)
    .fetch_optional(&mut **db)
    .await
    .map_err(map_sql)?
    else {
        return Ok(false);
    };

    let is_server_admin: bool = row.get("is_server_admin");
    let org_role: Option<String> = row.get("org_role");
    let Some(role) = stack_owner_access_role(Some(is_server_admin), org_role.as_deref()) else {
        return Ok(false);
    };
    let team_role: Option<String> = row.get("team_role");
    Ok(stack_owner_can_read_skill_with_role(
        &stack.owner_user_id,
        Some(role),
        team_role.as_deref(),
        skill,
    ))
}

fn stack_owner_access_role(
    is_server_admin: Option<bool>,
    org_role: Option<&str>,
) -> Option<AccessRole> {
    if is_server_admin == Some(true) {
        Some(AccessRole::ServerAdmin)
    } else {
        org_role.and_then(access_role_from_db)
    }
}

fn stack_owner_can_read_skill_with_role(
    stack_owner_user_id: &str,
    role: Option<AccessRole>,
    team_role: Option<&str>,
    skill: &SkillForStack,
) -> bool {
    let Some(role) = role else {
        return false;
    };
    match skill.visibility {
        Visibility::Org => role >= AccessRole::Reader,
        Visibility::Private => {
            role >= AccessRole::OrgAdmin
                || skill.owner_user_id.as_deref() == Some(stack_owner_user_id)
        }
        Visibility::Team => {
            role >= AccessRole::OrgAdmin
                || team_role.is_some()
                || skill.owner_user_id.as_deref() == Some(stack_owner_user_id)
        }
    }
}

fn access_role_from_db(value: &str) -> Option<AccessRole> {
    match value {
        "reader" => Some(AccessRole::Reader),
        "publisher" => Some(AccessRole::Publisher),
        "org_admin" => Some(AccessRole::OrgAdmin),
        _ => None,
    }
}

async fn load_pinned_version_id(
    db: &mut crate::db::DbTransaction<'_>,
    skill_id: &str,
    org: &str,
    skill: &str,
    version: &str,
) -> Result<String, ServerError> {
    let version_number = parse_version_number(version)?;
    sqlx::query_scalar(
        "SELECT id FROM skill_versions
         WHERE skill_id = $1 AND version_number = $2",
    )
    .bind(skill_id)
    .bind(version_number)
    .fetch_optional(&mut **db)
    .await
    .map_err(map_sql)?
    .ok_or_else(|| {
        ServerError::version_not_found(format!("no such version `{org}/{skill}@{version}`"))
    })
}

async fn next_stack_item_position_tx(
    db: &mut crate::db::DbTransaction<'_>,
    stack_id: &str,
) -> Result<i64, ServerError> {
    sqlx::query_scalar(
        "SELECT COALESCE(MAX(position), 0) + 1
         FROM stack_items
         WHERE stack_id = $1",
    )
    .bind(stack_id)
    .fetch_one(&mut **db)
    .await
    .map_err(map_sql)
}

async fn touch_stack_tx(
    db: &mut crate::db::DbTransaction<'_>,
    stack_id: &str,
) -> Result<(), ServerError> {
    sqlx::query(
        "UPDATE stacks
         SET updated_at = now()
         WHERE id = $1",
    )
    .bind(stack_id)
    .execute(&mut **db)
    .await
    .map_err(map_sql)?;
    Ok(())
}

fn version_policy_from_db(value: &str) -> Result<VersionPolicy, ServerError> {
    match value {
        "current" => Ok(VersionPolicy::Current),
        "pinned" => Ok(VersionPolicy::Pinned),
        _ => {
            tracing::error!(version_policy = value, "unknown version policy in database");
            Err(ServerError::internal_error())
        }
    }
}

fn version_status_from_db(value: &str) -> Result<VersionStatus, ServerError> {
    match value {
        "candidate" => Ok(VersionStatus::Candidate),
        "approved" => Ok(VersionStatus::Approved),
        "rejected" => Ok(VersionStatus::Rejected),
        _ => {
            tracing::error!(status = value, "unknown version status in database");
            Err(ServerError::internal_error())
        }
    }
}

fn stack_not_found(org: &str, stack: &str) -> ServerError {
    ServerError::stack_not_found(format!("no such stack `{org}/{stack}`"))
}

fn stack_resolution_failed() -> ServerError {
    ServerError::stack_resolution_failed(
        "stack cannot be resolved because at least one item is unavailable",
    )
}

#[derive(Debug, Clone, Deserialize)]
struct PushMetadata {
    name: String,
    description: String,
    org: String,
    visibility: Visibility,
    #[serde(default)]
    team: Option<String>,
    hash: PackageHash,
    #[serde(default)]
    platform_tags: Vec<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

#[derive(Debug, Serialize)]
struct PushResponse {
    metadata: SkillMetadata,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    skill_ref: String,
    version: String,
    sha256: String,
    visibility: Visibility,
    #[serde(skip_serializing_if = "Option::is_none")]
    audit_event_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct SkillStatusEnvelope {
    skill: RemoteSkill,
    versions: Vec<VersionInfo>,
}

#[derive(Debug, Serialize)]
struct SkillImpactEnvelope {
    skill: RemoteSkill,
    summary: SkillImpactSummary,
    used_by: Vec<SkillImpactStack>,
}

#[derive(Debug, Serialize)]
struct SkillImpactSummary {
    used_by_count: usize,
    current_policy_count: usize,
    pinned_count: usize,
    visible_only: bool,
}

#[derive(Debug, Serialize)]
struct SkillImpactStack {
    stack: String,
    org: String,
    slug: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner_email: Option<String>,
    visibility: Visibility,
    #[serde(skip_serializing_if = "Option::is_none")]
    team: Option<String>,
    version_policy: VersionPolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    pinned_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    effective_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<VersionStatus>,
    current: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    yanked_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    yank_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deprecated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deprecation_reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct VisibilityEnvelope {
    org: String,
    skill: String,
    visibility: Visibility,
    #[serde(skip_serializing_if = "Option::is_none")]
    team: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    audit_event_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct VisibilityPatchBody {
    visibility: Visibility,
    #[serde(default)]
    team: Option<String>,
}

#[derive(Debug, Serialize)]
struct AuditListEnvelope {
    events: Vec<AuditEvent>,
}

#[derive(Debug, Serialize)]
struct AuditEventEnvelope {
    event: AuditEvent,
}

#[derive(Debug, Serialize)]
struct SearchResult {
    org: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner_email: Option<String>,
    latest_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_version: Option<String>,
    description: String,
    visibility: Visibility,
    #[serde(skip_serializing_if = "Option::is_none")]
    team: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    platform_tags: Vec<String>,
    updated_at: String,
}

#[derive(Debug, Serialize)]
struct SkillList {
    skills: Vec<RemoteSkill>,
}

#[derive(Debug, Serialize)]
struct SearchResults {
    results: Vec<SearchResult>,
}

#[derive(Debug, Serialize)]
struct VersionList {
    versions: Vec<VersionInfo>,
}

#[derive(Debug, Default)]
struct ListQuery {
    q: Option<String>,
    query: Option<String>,
    search: Option<String>,
    org: Option<String>,
    team: Option<String>,
    platform: Vec<String>,
    visibility: Option<String>,
    owner: Option<String>,
    sort: Option<String>,
    limit: Option<String>,
}

impl ListQuery {
    fn from_pairs(pairs: Vec<(String, String)>) -> Self {
        let mut query = Self::default();
        for (key, value) in pairs {
            match key.as_str() {
                "q" if query.q.is_none() => query.q = Some(value),
                "query" if query.query.is_none() => query.query = Some(value),
                "search" if query.search.is_none() => query.search = Some(value),
                "org" if query.org.is_none() => query.org = Some(value),
                "team" if query.team.is_none() => query.team = Some(value),
                "platform" => query.platform.push(value),
                "visibility" if query.visibility.is_none() => query.visibility = Some(value),
                "owner" if query.owner.is_none() => query.owner = Some(value),
                "sort" if query.sort.is_none() => query.sort = Some(value),
                "limit" if query.limit.is_none() => query.limit = Some(value),
                _ => {}
            }
        }
        query
    }
}

async fn push_skill(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(org): Path<String>,
    RegistryMultipart(multipart): RegistryMultipart,
) -> Result<Json<PushResponse>, ServerError> {
    validate_slug(&org).map_err(ServerError::validation_error)?;
    let role = require_role(&user, &org, AccessRole::Reader).map_err(permission_denied)?;

    let (metadata, archive) = read_push_parts(multipart).await?;
    let archive = Arc::<[u8]>::from(archive);
    validate_push_metadata(&org, &metadata)?;

    if archive.len() > MAX_ARCHIVE_BYTES {
        return Err(ServerError::payload_too_large(format!(
            "archive is {} bytes; the limit is {MAX_ARCHIVE_BYTES}",
            archive.len()
        )));
    }

    // Authorize the publish before hashing/decompressing the archive: an unauthorized
    // caller must not be able to force a 50 MB sha256 + gzip/tar validation on every
    // request. The team role is needed for the visibility check, so resolve it first.
    let org_id = org_id_by_slug(&state.db, &org)
        .await?
        .ok_or_else(|| ServerError::validation_error(format!("unknown org `{org}`")))?;
    let (team_id, team_role) =
        resolve_push_team(&state.db, &user, &org_id, &org, &metadata).await?;
    if !can_publish_visibility(role, metadata.visibility, team_role.as_deref()) {
        return Err(forbidden_publish(role));
    }

    let actual_hash = sha256_hex(archive.as_ref());
    if metadata.hash.algorithm != "sha256" || metadata.hash.hex != actual_hash {
        return Err(ServerError::hash_mismatch(
            "archive bytes do not match metadata.hash",
        ));
    }
    if metadata.platform_tags.len() as i64 > state.quotas.max_platform_tags_per_version {
        return Err(ServerError::quota_exceeded(format!(
            "version has {} platform tags; the limit is {}",
            metadata.platform_tags.len(),
            state.quotas.max_platform_tags_per_version
        )));
    }
    validate_archive_metadata_blocking(
        Arc::clone(&archive),
        metadata.name.clone(),
        metadata.description.clone(),
    )
    .await?;

    let storage_key = storage_key_for_hash(&actual_hash);
    ensure_blob(&state, &storage_key, archive.as_ref()).await?;

    let mut tx = state.db.begin().await.map_err(map_sql)?;
    lock_skill_push(&mut tx, &org_id, &metadata.name).await?;

    let archive_id = ensure_archive_row(&mut tx, &actual_hash, &storage_key, archive.len()).await?;

    let skill_id = ensure_skill_row(
        &mut tx,
        &user,
        role,
        &state.quotas,
        &org_id,
        team_id.as_deref(),
        &metadata,
    )
    .await?;
    let version = next_version(&mut tx, &state.quotas, &skill_id).await?;
    insert_skill_version(
        &mut tx,
        &skill_id,
        &version,
        &archive_id,
        &metadata,
        &user.id,
    )
    .await?;
    touch_skill_updated_at(&mut tx, &skill_id).await?;
    let audit_event_id = insert_audit_log(
        &mut tx,
        &org_id,
        &user.id,
        "skill",
        &skill_id,
        "skill.version_pushed",
        serde_json::json!({ "version": version }),
    )
    .await?;
    tx.commit().await.map_err(map_sql)?;

    let stored = fetch_metadata(&state.db, &user, &org, &metadata.name, Some(&version))
        .await?
        .ok_or_else(ServerError::internal_error)?;
    let mut response_metadata = stored.metadata;
    response_metadata.audit_event_id = Some(audit_event_id.clone());
    let skill_ref = response_metadata.skill_ref();
    let version = response_metadata.version.clone();
    let sha256 = response_metadata.hash.hex.clone();
    let visibility = response_metadata.visibility;

    Ok(Json(PushResponse {
        skill_ref,
        version,
        sha256,
        visibility,
        metadata: response_metadata,
        url: None,
        audit_event_id: Some(audit_event_id),
    }))
}

async fn list_org_skills(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(org): Path<String>,
    RegistryQuery(params): RegistryQuery<Vec<(String, String)>>,
) -> Result<Json<SkillList>, ServerError> {
    let query = ListQuery::from_pairs(params);
    validate_slug(&org).map_err(ServerError::validation_error)?;
    require_role(&user, &org, AccessRole::Reader).map_err(permission_denied)?;
    let q = query.q.or(query.search).or(query.query);
    let platforms = normalized_platform_filters(&query.platform);
    let visibility = visibility_filter(query.visibility.as_deref())?;
    let owner = owner_filter(query.owner.as_deref());
    let sort = sort_filter(query.sort.as_deref())?;
    let limit = parse_limit(query.limit.as_deref())?;
    let mut skills = latest_skills_with_filters(
        &state.db,
        &user,
        SkillCatalogFilters {
            org: Some(&org),
            team: query.team.as_deref(),
            query: q.as_deref(),
            platforms: &platforms,
            visibility,
            owner: owner.as_deref(),
            sort,
            limit,
        },
    )
    .await?;
    apply_limit(&mut skills, limit);
    Ok(Json(SkillList { skills }))
}

async fn list_all_skills(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    RegistryQuery(params): RegistryQuery<Vec<(String, String)>>,
) -> Result<Json<SkillList>, ServerError> {
    let query = ListQuery::from_pairs(params);
    let q = query.q.or(query.search).or(query.query);
    let platforms = normalized_platform_filters(&query.platform);
    let visibility = visibility_filter(query.visibility.as_deref())?;
    let owner = owner_filter(query.owner.as_deref());
    let sort = sort_filter(query.sort.as_deref())?;
    let limit = parse_limit(query.limit.as_deref())?;
    let mut skills = latest_skills_with_filters(
        &state.db,
        &user,
        SkillCatalogFilters {
            org: query.org.as_deref(),
            team: query.team.as_deref(),
            query: q.as_deref(),
            platforms: &platforms,
            visibility,
            owner: owner.as_deref(),
            sort,
            limit,
        },
    )
    .await?;
    apply_limit(&mut skills, limit);
    Ok(Json(SkillList { skills }))
}

async fn search_skills(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    RegistryQuery(params): RegistryQuery<Vec<(String, String)>>,
) -> Result<Json<SearchResults>, ServerError> {
    let query = ListQuery::from_pairs(params);
    let q = query.q.or(query.search).or(query.query);
    let platforms = normalized_platform_filters(&query.platform);
    let visibility = visibility_filter(query.visibility.as_deref())?;
    let owner = owner_filter(query.owner.as_deref());
    let sort = sort_filter(query.sort.as_deref())?;
    let limit = parse_limit(query.limit.as_deref())?;
    let mut skills = latest_skills_with_filters(
        &state.db,
        &user,
        SkillCatalogFilters {
            org: query.org.as_deref(),
            team: query.team.as_deref(),
            query: q.as_deref(),
            platforms: &platforms,
            visibility,
            owner: owner.as_deref(),
            sort,
            limit,
        },
    )
    .await?;
    apply_limit(&mut skills, limit);
    let results = skills
        .into_iter()
        .map(|skill| SearchResult {
            org: skill.org,
            name: skill.name,
            owner_email: skill.owner_email,
            latest_version: skill.latest_version,
            current_version: skill.current_version,
            description: skill.description,
            visibility: skill.visibility,
            team: skill.team,
            platform_tags: skill.platform_tags,
            updated_at: skill.updated_at,
        })
        .collect();
    Ok(Json(SearchResults { results }))
}

fn parse_limit(value: Option<&str>) -> Result<Option<usize>, ServerError> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let limit = value.parse::<usize>().map_err(|_| {
        ServerError::validation_error("limit must be a non-negative integer".to_string())
    })?;
    if limit > 1000 {
        return Err(ServerError::validation_error(
            "limit must be 1000 or less".to_string(),
        ));
    }
    Ok(Some(limit))
}

fn apply_limit<T>(rows: &mut Vec<T>, limit: Option<usize>) {
    if let Some(limit) = limit {
        rows.truncate(limit);
    }
}

fn normalized_platform_filters(values: &[String]) -> Vec<String> {
    values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn owner_filter(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
}

fn sort_filter(value: Option<&str>) -> Result<Option<CatalogSort>, ServerError> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| CatalogSort::parse(value).map_err(ServerError::validation_error))
        .transpose()
}

fn visibility_filter(value: Option<&str>) -> Result<Option<Visibility>, ServerError> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| match value {
            "private" => Ok(Visibility::Private),
            "org" => Ok(Visibility::Org),
            "team" => Ok(Visibility::Team),
            other => Err(ServerError::validation_error(format!(
                "unknown visibility `{other}` (expected one of: private, org, team)"
            ))),
        })
        .transpose()
}

async fn get_latest_skill(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((org, skill)): Path<(String, String)>,
) -> Result<Json<SkillMetadata>, ServerError> {
    validate_slug(&org).map_err(ServerError::validation_error)?;
    validate_slug(&skill).map_err(ServerError::validation_error)?;
    require_role(&user, &org, AccessRole::Reader).map_err(permission_denied)?;

    let stored = fetch_metadata(&state.db, &user, &org, &skill, None)
        .await?
        .ok_or_else(|| ServerError::skill_not_found(format!("no such skill `{org}/{skill}`")))?;
    Ok(Json(stored.metadata))
}

async fn skill_status(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((org, skill)): Path<(String, String)>,
) -> Result<Json<SkillStatusEnvelope>, ServerError> {
    validate_slug(&org).map_err(ServerError::validation_error)?;
    validate_slug(&skill).map_err(ServerError::validation_error)?;
    require_role(&user, &org, AccessRole::Reader).map_err(permission_denied)?;

    let versions = visible_versions(&state.db, &user, &org, &skill).await?;
    let skill_summary = match load_visible_skill_summary(&state.db, &user, &org, &skill).await {
        Ok(summary) => summary,
        Err(err) if err.code() == "skill_not_found" && !versions.is_empty() => {
            load_yanked_visible_skill_summary(&state.db, &org, &skill, &versions).await?
        }
        Err(err) => return Err(err),
    };
    Ok(Json(SkillStatusEnvelope {
        skill: skill_summary,
        versions,
    }))
}

async fn skill_impact(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((org, skill)): Path<(String, String)>,
) -> Result<Json<SkillImpactEnvelope>, ServerError> {
    validate_slug(&org).map_err(ServerError::validation_error)?;
    validate_slug(&skill).map_err(ServerError::validation_error)?;
    let role = require_role(&user, &org, AccessRole::Reader).map_err(permission_denied)?;

    let skill_summary = match load_visible_skill_summary(&state.db, &user, &org, &skill).await {
        Ok(summary) => summary,
        Err(err) if err.code() == "skill_not_found" => {
            let versions = visible_versions(&state.db, &user, &org, &skill).await?;
            if versions.is_empty() {
                return Err(err);
            }
            load_yanked_visible_skill_summary(&state.db, &org, &skill, &versions).await?
        }
        Err(err) => return Err(err),
    };
    let rows = sqlx::query(
        "SELECT stacks.slug, stacks.name, stacks.visibility,
                teams.slug AS team_slug,
                stacks.owner_user_id AS stack_owner_user_id,
                owner_users.email AS owner_email,
                (SELECT team_memberships.role
                 FROM team_memberships
                 WHERE team_memberships.team_id = stacks.team_id
                   AND team_memberships.user_id = $1) AS team_role,
                skills.visibility AS skill_visibility,
                skills.owner_user_id AS skill_owner_user_id,
                (SELECT team_memberships.role
                 FROM team_memberships
                 WHERE team_memberships.team_id = skills.team_id
                   AND team_memberships.user_id = $1) AS skill_team_role,
                stack_items.version_policy,
                pinned_versions.version_number::text AS pinned_version,
                selected_versions.version_number::text AS effective_version,
                selected_versions.status,
                selected_versions.id = skills.current_version_id AS current,
                to_char(selected_versions.yanked_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS yanked_at,
                selected_versions.yank_reason,
                to_char(selected_versions.deprecated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS deprecated_at,
                selected_versions.deprecation_reason
         FROM stack_items
         JOIN stacks ON stacks.id = stack_items.stack_id
         JOIN users AS owner_users ON owner_users.id = stacks.owner_user_id
         LEFT JOIN teams ON teams.id = stacks.team_id
         JOIN skills ON skills.id = stack_items.skill_id
         JOIN orgs ON orgs.id = skills.org_id
         LEFT JOIN skill_versions AS pinned_versions
           ON pinned_versions.id = stack_items.pinned_version_id
         LEFT JOIN skill_versions AS selected_versions
           ON selected_versions.id = CASE
                WHEN stack_items.version_policy = 'current' THEN skills.current_version_id
                ELSE stack_items.pinned_version_id
              END
         WHERE orgs.slug = $2 AND skills.name = $3
         ORDER BY stacks.slug ASC",
    )
    .bind(&user.id)
    .bind(&org)
    .bind(&skill)
    .fetch_all(&state.db)
    .await
    .map_err(map_sql)?;

    let mut used_by = Vec::new();
    let mut current_policy_count = 0;
    let mut pinned_count = 0;
    for row in rows {
        let visibility = visibility_from_db(row.get::<String, _>("visibility").as_str())?;
        let stack_owner_user_id: String = row.get("stack_owner_user_id");
        let team_role: Option<String> = row.get("team_role");
        if !can_read_visibility(
            &user,
            role,
            visibility,
            Some(stack_owner_user_id.as_str()),
            team_role.is_some(),
        ) {
            continue;
        }

        let version_policy =
            version_policy_from_db(row.get::<String, _>("version_policy").as_str())?;
        match version_policy {
            VersionPolicy::Current => current_policy_count += 1,
            VersionPolicy::Pinned => pinned_count += 1,
        }
        let status = row
            .get::<Option<String>, _>("status")
            .map(|status| version_status_from_db(status.as_str()))
            .transpose()?;
        let version_visible = status
            .map(|status| {
                let skill_visibility =
                    visibility_from_db(row.get::<String, _>("skill_visibility").as_str())?;
                let skill_owner_user_id: String = row.get("skill_owner_user_id");
                let skill_team_role: Option<String> = row.get("skill_team_role");
                Ok(can_read_version(
                    &user,
                    role,
                    skill_visibility,
                    Some(skill_owner_user_id.as_str()),
                    skill_team_role.as_deref(),
                    status,
                ))
            })
            .transpose()?
            .unwrap_or(false);
        let slug: String = row.get("slug");
        used_by.push(SkillImpactStack {
            stack: format!("{org}/{slug}"),
            org: org.clone(),
            slug,
            name: row.get("name"),
            owner_email: row.get("owner_email"),
            visibility,
            team: row.get("team_slug"),
            version_policy,
            pinned_version: version_visible.then(|| row.get("pinned_version")).flatten(),
            effective_version: version_visible
                .then(|| row.get("effective_version"))
                .flatten(),
            status: version_visible.then_some(status).flatten(),
            current: version_visible && row.get::<Option<bool>, _>("current").unwrap_or(false),
            yanked_at: version_visible.then(|| row.get("yanked_at")).flatten(),
            yank_reason: version_visible.then(|| row.get("yank_reason")).flatten(),
            deprecated_at: version_visible.then(|| row.get("deprecated_at")).flatten(),
            deprecation_reason: version_visible
                .then(|| row.get("deprecation_reason"))
                .flatten(),
        });
    }

    Ok(Json(SkillImpactEnvelope {
        skill: skill_summary,
        summary: SkillImpactSummary {
            used_by_count: used_by.len(),
            current_policy_count,
            pinned_count,
            visible_only: true,
        },
        used_by,
    }))
}

async fn skill_visibility(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((org, skill)): Path<(String, String)>,
) -> Result<Json<VisibilityEnvelope>, ServerError> {
    validate_slug(&org).map_err(ServerError::validation_error)?;
    validate_slug(&skill).map_err(ServerError::validation_error)?;
    require_role(&user, &org, AccessRole::Reader).map_err(permission_denied)?;

    let skill_summary = load_visible_skill_summary(&state.db, &user, &org, &skill).await?;
    Ok(Json(VisibilityEnvelope {
        org,
        skill,
        visibility: skill_summary.visibility,
        team: skill_summary.team,
        audit_event_id: None,
    }))
}

async fn patch_skill_visibility(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((org, skill)): Path<(String, String)>,
    RegistryJson(body): RegistryJson<VisibilityPatchBody>,
) -> Result<Json<VisibilityEnvelope>, ServerError> {
    validate_slug(&org).map_err(ServerError::validation_error)?;
    validate_slug(&skill).map_err(ServerError::validation_error)?;
    let role = require_role(&user, &org, AccessRole::Reader).map_err(permission_denied)?;

    let mut tx = state.db.begin().await.map_err(map_sql)?;
    let row = sqlx::query(
        "SELECT orgs.id AS org_id, skills.id AS skill_id, skills.visibility,
                skills.owner_user_id,
                teams.slug AS team_slug,
                (SELECT team_memberships.role
                 FROM team_memberships
                 WHERE team_memberships.team_id = skills.team_id
                   AND team_memberships.user_id = $1) AS team_role
         FROM skills
         JOIN orgs ON orgs.id = skills.org_id
         LEFT JOIN teams ON teams.id = skills.team_id
         WHERE orgs.slug = $2 AND skills.name = $3",
    )
    .bind(&user.id)
    .bind(&org)
    .bind(&skill)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sql)?
    .ok_or_else(|| ServerError::skill_not_found(format!("no such skill `{org}/{skill}`")))?;

    let org_id: String = row.get("org_id");
    let skill_id: String = row.get("skill_id");
    let previous_visibility = visibility_from_db(row.get::<String, _>("visibility").as_str())?;
    let owner_user_id: String = row.get("owner_user_id");
    let previous_team: Option<String> = row.get("team_slug");
    let team_role: Option<String> = row.get("team_role");
    if !can_read_visibility(
        &user,
        role,
        previous_visibility,
        Some(owner_user_id.as_str()),
        team_role.is_some(),
    ) {
        return Err(ServerError::skill_not_found(format!(
            "no such skill `{org}/{skill}`"
        )));
    }
    if !can_manage_skill(role, previous_visibility, team_role.as_deref()) {
        return Err(forbidden_manage(role));
    }

    let team_slug = if body.visibility == Visibility::Team {
        Some(body.team.as_deref().ok_or_else(|| {
            ServerError::validation_error("skill.team is required with visibility `team`")
        })?)
    } else {
        if body.team.is_some() {
            return Err(ServerError::validation_error(
                "skill.team is only valid with visibility `team`",
            ));
        }
        None
    };
    let changed = previous_visibility != body.visibility || previous_team.as_deref() != team_slug;
    if changed && role < AccessRole::OrgAdmin {
        return Err(ServerError::forbidden("permission denied"));
    }
    let (team_id, target_team_role) = resolve_resource_team(
        &state.db,
        &user,
        &org_id,
        &org,
        body.visibility,
        team_slug,
        "skill",
    )
    .await?;
    if !can_publish_visibility(role, body.visibility, target_team_role.as_deref()) {
        return Err(forbidden_publish(role));
    }

    if changed {
        sqlx::query(
            "UPDATE skills
             SET visibility = $1,
                 team_id = $2,
                 updated_at = now()
             WHERE id = $3",
        )
        .bind(body.visibility.as_str())
        .bind(team_id.as_deref())
        .bind(&skill_id)
        .execute(&mut *tx)
        .await
        .map_err(map_sql)?;
    }

    let audit_event_id = insert_audit_log(
        &mut tx,
        &org_id,
        &user.id,
        "skill",
        &skill_id,
        "skill.visibility_changed",
        serde_json::json!({
            "previous_visibility": previous_visibility.as_str(),
            "previous_team": previous_team,
            "visibility": body.visibility.as_str(),
            "team": team_slug,
            "role": role.as_str(),
            "changed": changed,
        }),
    )
    .await?;
    tx.commit().await.map_err(map_sql)?;

    Ok(Json(VisibilityEnvelope {
        org,
        skill,
        visibility: body.visibility,
        team: team_slug.map(str::to_string),
        audit_event_id: Some(audit_event_id),
    }))
}

async fn skill_audit(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((org, skill)): Path<(String, String)>,
) -> Result<Json<AuditListEnvelope>, ServerError> {
    validate_slug(&org).map_err(ServerError::validation_error)?;
    validate_slug(&skill).map_err(ServerError::validation_error)?;
    require_role(&user, &org, AccessRole::OrgAdmin).map_err(permission_denied)?;
    let skill_id = resource_id_by_slug(&state.db, &org, "skill", &skill)
        .await?
        .ok_or_else(|| ServerError::skill_not_found(format!("no such skill `{org}/{skill}`")))?;
    let events = list_audit_events(
        &state.db,
        &org,
        Some(("skill", skill_id.as_str())),
        None,
        Some(MAX_HTTP_AUDIT_EVENTS),
    )
    .await
    .map_err(map_sql)?;
    Ok(Json(AuditListEnvelope { events }))
}

async fn get_skill_version(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((org, skill, version)): Path<(String, String, String)>,
) -> Result<Json<SkillMetadata>, ServerError> {
    validate_slug(&org).map_err(ServerError::validation_error)?;
    validate_slug(&skill).map_err(ServerError::validation_error)?;
    require_role(&user, &org, AccessRole::Reader).map_err(permission_denied)?;

    let stored = fetch_metadata(&state.db, &user, &org, &skill, Some(&version))
        .await?
        .ok_or_else(|| {
            ServerError::version_not_found(format!("no such version `{org}/{skill}@{version}`"))
        })?;
    Ok(Json(stored.metadata))
}

async fn list_skill_versions(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((org, skill)): Path<(String, String)>,
) -> Result<Json<VersionList>, ServerError> {
    validate_slug(&org).map_err(ServerError::validation_error)?;
    validate_slug(&skill).map_err(ServerError::validation_error)?;
    require_role(&user, &org, AccessRole::Reader).map_err(permission_denied)?;
    let versions = visible_versions(&state.db, &user, &org, &skill).await?;
    Ok(Json(VersionList { versions }))
}

#[derive(Debug, Deserialize, Default)]
struct ArchiveQuery {
    #[serde(default)]
    allow_yanked: Option<String>,
}

async fn get_skill_archive(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((org, skill, version)): Path<(String, String, String)>,
    RegistryQuery(query): RegistryQuery<ArchiveQuery>,
) -> Result<Response, ServerError> {
    validate_slug(&org).map_err(ServerError::validation_error)?;
    validate_slug(&skill).map_err(ServerError::validation_error)?;
    let role = require_role(&user, &org, AccessRole::Reader).map_err(permission_denied)?;

    let stored = fetch_metadata(&state.db, &user, &org, &skill, Some(&version))
        .await?
        .ok_or_else(|| {
            ServerError::version_not_found(format!("no such version `{org}/{skill}@{version}`"))
        })?;

    if stored.metadata.yanked_at.is_some() {
        if !is_truthy(query.allow_yanked.as_deref()) {
            return Err(ServerError::version_yanked(format!(
                "`{org}/{skill}@{version}` was yanked"
            )));
        }
        if role != AccessRole::ServerAdmin {
            return Err(ServerError::forbidden(
                "only server admins can recover yanked archives",
            ));
        }
    }
    let bytes = state
        .blob_store
        .get(&stored.storage_key)
        .await
        .map_err(|err| map_blob_error(err, "failed to read archive blob"))?;

    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/gzip"));
    headers.insert(
        "x-agentstack-sha256",
        HeaderValue::from_str(&stored.metadata.hash.hex)
            .map_err(|_| ServerError::internal_error())?,
    );
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "attachment; filename=\"{org}-{skill}-{version}.tar.gz\""
        ))
        .map_err(|_| ServerError::internal_error())?,
    );
    Ok((StatusCode::OK, headers, Body::from(bytes)).into_response())
}

async fn approve_skill_version(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((org, skill, version)): Path<(String, String, String)>,
) -> Result<Json<SkillMetadata>, ServerError> {
    validate_slug(&org).map_err(ServerError::validation_error)?;
    validate_slug(&skill).map_err(ServerError::validation_error)?;
    let role = require_role(&user, &org, AccessRole::Reader).map_err(permission_denied)?;
    let version_number = parse_version_number(&version)?;

    let mut tx = state.db.begin().await.map_err(map_sql)?;

    let row = sqlx::query(
        "SELECT orgs.id AS org_id, skills.id AS skill_id, skills.current_version_id,
                skills.visibility,
                skills.owner_user_id,
                (SELECT team_memberships.role
                 FROM team_memberships
                 WHERE team_memberships.team_id = skills.team_id
                   AND team_memberships.user_id = $1) AS team_role,
                skill_versions.id AS version_id,
                skill_versions.description AS version_description,
                skill_versions.yanked_at IS NOT NULL AS was_yanked,
                skill_versions.deprecated_at IS NOT NULL AS was_deprecated
         FROM skills
         JOIN orgs ON orgs.id = skills.org_id
         JOIN skill_versions ON skill_versions.skill_id = skills.id
         WHERE orgs.slug = $2 AND skills.name = $3 AND skill_versions.version_number = $4",
    )
    .bind(&user.id)
    .bind(&org)
    .bind(&skill)
    .bind(version_number)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sql)?
    .ok_or_else(|| {
        ServerError::version_not_found(format!("no such version `{org}/{skill}@{version}`"))
    })?;

    let org_id: String = row.get("org_id");
    let skill_id: String = row.get("skill_id");
    let version_id: String = row.get("version_id");
    let version_description: String = row.get("version_description");
    let previous_current: Option<String> = row.get("current_version_id");
    let was_yanked: bool = row.get("was_yanked");
    let was_deprecated: bool = row.get("was_deprecated");
    let visibility = visibility_from_db(row.get::<String, _>("visibility").as_str())?;
    let owner_user_id: String = row.get("owner_user_id");
    let team_role: Option<String> = row.get("team_role");
    if !can_read_visibility(
        &user,
        role,
        visibility,
        Some(owner_user_id.as_str()),
        team_role.is_some(),
    ) {
        return Err(ServerError::version_not_found(format!(
            "no such version `{org}/{skill}@{version}`"
        )));
    }
    if !can_manage_skill(role, visibility, team_role.as_deref()) {
        return Err(forbidden_manage(role));
    }

    sqlx::query(
        "UPDATE skill_versions
         SET status = 'approved',
             approved_by_user_id = $1,
             approved_at = now(),
             yanked_at = NULL,
             yanked_by_user_id = NULL,
             yank_reason = NULL,
             deprecated_at = NULL,
             deprecated_by_user_id = NULL,
             deprecation_reason = NULL
         WHERE id = $2",
    )
    .bind(&user.id)
    .bind(&version_id)
    .execute(&mut *tx)
    .await
    .map_err(map_sql)?;

    sqlx::query(
        "UPDATE skills
         SET current_version_id = $1,
             description = $2,
             updated_at = now()
         WHERE id = $3",
    )
    .bind(&version_id)
    .bind(&version_description)
    .bind(&skill_id)
    .execute(&mut *tx)
    .await
    .map_err(map_sql)?;

    let audit_event_id = insert_audit_log(
        &mut tx,
        &org_id,
        &user.id,
        "skill",
        &skill_id,
        "skill.version_approved",
        serde_json::json!({ "version": version, "role": role.as_str() }),
    )
    .await?;
    if was_yanked {
        insert_audit_log(
            &mut tx,
            &org_id,
            &user.id,
            "skill",
            &skill_id,
            "skill.version_unyanked",
            serde_json::json!({ "version": version, "role": role.as_str() }),
        )
        .await?;
    }
    if was_deprecated {
        insert_audit_log(
            &mut tx,
            &org_id,
            &user.id,
            "skill",
            &skill_id,
            "skill.version_undeprecated",
            serde_json::json!({ "version": version, "role": role.as_str() }),
        )
        .await?;
    }
    if previous_current.as_deref() != Some(version_id.as_str()) {
        insert_audit_log(
            &mut tx,
            &org_id,
            &user.id,
            "skill",
            &skill_id,
            "skill.current_changed",
            serde_json::json!({
                "version": version,
                "previous_current_version_id": previous_current,
                "current_version_id": version_id,
            }),
        )
        .await?;
    }

    tx.commit().await.map_err(map_sql)?;

    let stored = fetch_metadata(&state.db, &user, &org, &skill, Some(&version))
        .await?
        .ok_or_else(ServerError::internal_error)?;
    let mut metadata = stored.metadata;
    metadata.audit_event_id = Some(audit_event_id);
    Ok(Json(metadata))
}

fn can_manage_skill(role: AccessRole, visibility: Visibility, team_role: Option<&str>) -> bool {
    role >= AccessRole::OrgAdmin
        || (visibility == Visibility::Team && is_team_admin_role(team_role))
}

#[derive(Debug, Deserialize)]
struct LifecycleRequest {
    #[serde(default)]
    reason: String,
}

async fn yank_skill_version(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((org, skill, version)): Path<(String, String, String)>,
    RegistryJson(body): RegistryJson<LifecycleRequest>,
) -> Result<Json<SkillMetadata>, ServerError> {
    lifecycle_action(&state, &user, &org, &skill, &version, body, Lifecycle::Yank).await
}

async fn deprecate_skill_version(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((org, skill, version)): Path<(String, String, String)>,
    RegistryJson(body): RegistryJson<LifecycleRequest>,
) -> Result<Json<SkillMetadata>, ServerError> {
    lifecycle_action(
        &state,
        &user,
        &org,
        &skill,
        &version,
        body,
        Lifecycle::Deprecate,
    )
    .await
}

#[derive(Clone, Copy)]
enum Lifecycle {
    Yank,
    Deprecate,
}

impl Lifecycle {
    fn audit_action(self) -> &'static str {
        match self {
            Self::Yank => "skill.version_yanked",
            Self::Deprecate => "skill.version_deprecated",
        }
    }
}

async fn lifecycle_action(
    state: &AppState,
    user: &AuthenticatedUser,
    org: &str,
    skill: &str,
    version: &str,
    body: LifecycleRequest,
    action: Lifecycle,
) -> Result<Json<SkillMetadata>, ServerError> {
    validate_slug(org).map_err(ServerError::validation_error)?;
    validate_slug(skill).map_err(ServerError::validation_error)?;
    let role = require_role(user, org, AccessRole::Reader).map_err(permission_denied)?;
    let version_number = parse_version_number(version)?;

    let mut tx = state.db.begin().await.map_err(map_sql)?;

    let row = sqlx::query(
        "SELECT orgs.id AS org_id, skills.id AS skill_id,
                skills.current_version_id,
                skills.visibility,
                skills.owner_user_id,
                (SELECT team_memberships.role
                 FROM team_memberships
                 WHERE team_memberships.team_id = skills.team_id
                   AND team_memberships.user_id = $1) AS team_role,
                skill_versions.id AS version_id,
                to_char(skill_versions.yanked_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS yanked_at,
                to_char(skill_versions.deprecated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS deprecated_at
         FROM skills
         JOIN orgs ON orgs.id = skills.org_id
         JOIN skill_versions ON skill_versions.skill_id = skills.id
         WHERE orgs.slug = $2 AND skills.name = $3 AND skill_versions.version_number = $4",
    )
    .bind(&user.id)
    .bind(org)
    .bind(skill)
    .bind(version_number)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sql)?
    .ok_or_else(|| {
        ServerError::version_not_found(format!("no such version `{org}/{skill}@{version}`"))
    })?;

    let org_id: String = row.get("org_id");
    let skill_id: String = row.get("skill_id");
    let version_id: String = row.get("version_id");
    let current_version_id: Option<String> = row.get("current_version_id");
    let yanked_at: Option<String> = row.get("yanked_at");
    let deprecated_at: Option<String> = row.get("deprecated_at");
    let visibility = visibility_from_db(row.get::<String, _>("visibility").as_str())?;
    let owner_user_id: String = row.get("owner_user_id");
    let team_role: Option<String> = row.get("team_role");
    if !can_read_visibility(
        user,
        role,
        visibility,
        Some(owner_user_id.as_str()),
        team_role.is_some(),
    ) {
        return Err(ServerError::version_not_found(format!(
            "no such version `{org}/{skill}@{version}`"
        )));
    }
    if !can_manage_skill(role, visibility, team_role.as_deref()) {
        return Err(forbidden_manage(role));
    }

    let reason = validate_lifecycle_reason(&body.reason)?;

    match action {
        Lifecycle::Yank => {
            if yanked_at.is_some() {
                return Err(ServerError::already_yanked(format!(
                    "`{org}/{skill}@{version}` is already yanked"
                )));
            }
            sqlx::query(
                "UPDATE skill_versions
                 SET yanked_by_user_id = $1,
                     yanked_at = now(),
                     yank_reason = $2
                 WHERE id = $3",
            )
            .bind(&user.id)
            .bind(&reason)
            .bind(&version_id)
            .execute(&mut *tx)
            .await
            .map_err(map_sql)?;

            if current_version_id.as_deref() == Some(version_id.as_str()) {
                sqlx::query(
                    "UPDATE skills
                     SET current_version_id = NULL,
                         updated_at = now()
                     WHERE id = $1",
                )
                .bind(&skill_id)
                .execute(&mut *tx)
                .await
                .map_err(map_sql)?;

                insert_audit_log(
                    &mut tx,
                    &org_id,
                    &user.id,
                    "skill",
                    &skill_id,
                    "skill.current_changed",
                    serde_json::json!({
                        "version": version,
                        "previous_current_version_id": version_id,
                        "current_version_id": null,
                    }),
                )
                .await?;
            }
        }
        Lifecycle::Deprecate => {
            if deprecated_at.is_some() {
                return Err(ServerError::already_deprecated(format!(
                    "`{org}/{skill}@{version}` is already deprecated"
                )));
            }
            sqlx::query(
                "UPDATE skill_versions
                 SET deprecated_by_user_id = $1,
                     deprecated_at = now(),
                     deprecation_reason = $2
                 WHERE id = $3",
            )
            .bind(&user.id)
            .bind(&reason)
            .bind(&version_id)
            .execute(&mut *tx)
            .await
            .map_err(map_sql)?;
        }
    }

    let audit_event_id = insert_audit_log(
        &mut tx,
        &org_id,
        &user.id,
        "skill",
        &skill_id,
        action.audit_action(),
        serde_json::json!({
            "version": version,
            "role": role.as_str(),
            "reason": reason,
        }),
    )
    .await?;
    tx.commit().await.map_err(map_sql)?;

    let stored = fetch_metadata(&state.db, user, org, skill, Some(version))
        .await?
        .ok_or_else(ServerError::internal_error)?;
    let mut metadata = stored.metadata;
    metadata.audit_event_id = Some(audit_event_id);
    Ok(Json(metadata))
}

fn validate_lifecycle_reason(reason: &str) -> Result<String, ServerError> {
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(ServerError::validation_error("reason must not be empty"));
    }
    if contains_token_like_secret(reason) {
        return Err(ServerError::validation_error(
            "reason must not contain bearer tokens or raw AgentStack tokens",
        ));
    }
    Ok(reason.to_string())
}

fn contains_token_like_secret(value: &str) -> bool {
    has_bearer_token(value) || has_agentstack_token(value)
}

fn has_bearer_token(value: &str) -> bool {
    let mut words = value.split_whitespace();
    while let Some(word) = words.next() {
        if word.eq_ignore_ascii_case("bearer") && words.next().is_some() {
            return true;
        }
    }
    false
}

/// `adk_` prefix (4 bytes) followed by 64 lowercase hex characters.
const AGENTSTACK_TOKEN_LEN: usize = 4 + 64;

fn has_agentstack_token(value: &str) -> bool {
    let bytes = value.as_bytes();
    for start in 0..bytes.len() {
        if !bytes[start..].starts_with(b"adk_") {
            continue;
        }
        let token_end = start + AGENTSTACK_TOKEN_LEN;
        if token_end > bytes.len() {
            continue;
        }
        if bytes[start + 4..token_end]
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return true;
        }
    }
    false
}

fn is_truthy(value: Option<&str>) -> bool {
    matches!(value, Some(v) if matches!(v, "1" | "true" | "yes" | "on"))
}

async fn read_push_parts(mut multipart: Multipart) -> Result<(PushMetadata, Vec<u8>), ServerError> {
    let mut metadata = None;
    let mut archive = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| ServerError::bad_request("malformed multipart request"))?
    {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "metadata" => {
                if metadata.is_some() {
                    return Err(ServerError::bad_request("duplicate metadata part"));
                }
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|_| ServerError::bad_request("failed to read metadata part"))?;
                let parsed = serde_json::from_slice(&bytes)
                    .map_err(|_| ServerError::bad_request("metadata part is not valid JSON"))?;
                metadata = Some(parsed);
            }
            "archive" => {
                if archive.is_some() {
                    return Err(ServerError::bad_request("duplicate archive part"));
                }
                let mut field = field;
                let mut bytes = Vec::new();
                while let Some(chunk) = field
                    .chunk()
                    .await
                    .map_err(|_| ServerError::bad_request("failed to read archive part"))?
                {
                    let next_len = bytes.len().checked_add(chunk.len()).ok_or_else(|| {
                        ServerError::payload_too_large("archive exceeds size limit")
                    })?;
                    if next_len > MAX_ARCHIVE_BYTES {
                        return Err(ServerError::payload_too_large(format!(
                            "archive exceeds size limit of {MAX_ARCHIVE_BYTES} bytes"
                        )));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                archive = Some(bytes);
            }
            "" => return Err(ServerError::bad_request("multipart part is missing a name")),
            other => {
                return Err(ServerError::bad_request(format!(
                    "unexpected multipart field `{other}`"
                )));
            }
        }
    }

    let metadata = metadata.ok_or_else(|| ServerError::bad_request("missing metadata part"))?;
    let archive = archive.ok_or_else(|| ServerError::bad_request("missing archive part"))?;
    Ok((metadata, archive))
}

fn validate_push_metadata(org: &str, metadata: &PushMetadata) -> Result<(), ServerError> {
    validate_slug(&metadata.name).map_err(ServerError::validation_error)?;
    validate_slug(&metadata.org).map_err(ServerError::validation_error)?;
    if metadata.org != org {
        return Err(ServerError::validation_error(format!(
            "metadata org `{}` does not match path org `{org}`",
            metadata.org
        )));
    }
    if metadata.description.trim().is_empty() {
        return Err(ServerError::validation_error(
            "metadata.description must not be empty",
        ));
    }
    match metadata.visibility {
        Visibility::Private | Visibility::Org => {
            if metadata.team.is_some() {
                return Err(ServerError::validation_error(
                    "metadata.team is only valid with visibility `team`",
                ));
            }
        }
        Visibility::Team => {
            let team = metadata.team.as_deref().ok_or_else(|| {
                ServerError::validation_error("metadata.team is required with visibility `team`")
            })?;
            validate_slug(team).map_err(ServerError::validation_error)?;
        }
    }
    if metadata.created_at.is_some() || metadata.updated_at.is_some() {
        return Err(ServerError::validation_error(
            "created_at and updated_at are server-assigned",
        ));
    }
    if !is_sha256_hex(&metadata.hash.hex) {
        return Err(ServerError::validation_error(
            "metadata.hash.hex must be 64 lowercase hex characters",
        ));
    }
    for tag in &metadata.platform_tags {
        if !is_valid_platform_tag(tag) {
            return Err(ServerError::validation_error(format!(
                "metadata.platform_tags entry `{tag}` must match ^[a-z0-9][a-z0-9._-]*$",
            )));
        }
    }
    Ok(())
}

fn is_valid_platform_tag(tag: &str) -> bool {
    let mut chars = tag.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    chars.all(|ch| {
        ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '.' || ch == '_' || ch == '-'
    })
}

async fn ensure_blob(state: &AppState, key: &str, bytes: &[u8]) -> Result<(), ServerError> {
    let exists = state
        .blob_store
        .exists(key)
        .await
        .map_err(|err| map_blob_error(err, "failed to check archive blob"))?;
    if exists {
        // The key is derived from the validated SHA-256, so reusing an existing
        // object avoids a full blob read on repeated pushes. Failed metadata
        // writes may leave content-addressed orphans for a later cleanup job.
        return Ok(());
    }
    state
        .blob_store
        .put(key, bytes)
        .await
        .map_err(|err| map_blob_error(err, "failed to store archive blob"))?;
    Ok(())
}

fn map_blob_error(err: BlobStoreError, context: &'static str) -> ServerError {
    match err {
        BlobStoreError::NotFound => ServerError::version_not_found("archive blob not found"),
        other => {
            tracing::error!(error = %other, "{context}");
            ServerError::internal_error()
        }
    }
}

async fn ensure_archive_row(
    db: &mut crate::db::DbTransaction<'_>,
    hash_hex: &str,
    storage_key: &str,
    size_bytes: usize,
) -> Result<String, ServerError> {
    if let Some(row) = sqlx::query(
        "SELECT id FROM archives
         WHERE hash_algorithm = 'sha256' AND hash_hex = $1",
    )
    .bind(hash_hex)
    .fetch_optional(&mut **db)
    .await
    .map_err(map_sql)?
    {
        return Ok(row.get("id"));
    }

    let id = random_id("arc");
    sqlx::query(
        "INSERT INTO archives
            (id, hash_algorithm, hash_hex, storage_key, size_bytes)
         VALUES
            ($1, 'sha256', $2, $3, $4)",
    )
    .bind(&id)
    .bind(hash_hex)
    .bind(storage_key)
    .bind(size_bytes as i64)
    .execute(&mut **db)
    .await
    .map_err(map_sql)?;
    Ok(id)
}

async fn resolve_push_team(
    db: &DbPool,
    user: &AuthenticatedUser,
    org_id: &str,
    org: &str,
    metadata: &PushMetadata,
) -> Result<(Option<String>, Option<String>), ServerError> {
    let Some(team) = metadata.team.as_deref() else {
        return Ok((None, None));
    };

    let row = sqlx::query(
        "SELECT teams.id AS team_id,
                (SELECT team_memberships.role
                 FROM team_memberships
                 WHERE team_memberships.team_id = teams.id
                   AND team_memberships.user_id = $1) AS team_role
         FROM teams
         WHERE teams.org_id = $2 AND teams.slug = $3",
    )
    .bind(&user.id)
    .bind(org_id)
    .bind(team)
    .fetch_optional(db)
    .await
    .map_err(map_sql)?
    .ok_or_else(|| ServerError::validation_error(format!("unknown team `{org}/{team}`")))?;

    Ok((Some(row.get("team_id")), row.get("team_role")))
}

async fn ensure_skill_row(
    db: &mut crate::db::DbTransaction<'_>,
    user: &AuthenticatedUser,
    role: AccessRole,
    quotas: &QuotaConfig,
    org_id: &str,
    team_id: Option<&str>,
    metadata: &PushMetadata,
) -> Result<String, ServerError> {
    if let Some(id) = lookup_existing_skill(db, user, role, org_id, metadata).await? {
        return Ok(id);
    }

    sqlx::query("SELECT id FROM orgs WHERE id = $1 FOR UPDATE")
        .bind(org_id)
        .fetch_one(&mut **db)
        .await
        .map_err(map_sql)?;

    enforce_new_skill_quotas(db, quotas, org_id, &user.id, team_id, &metadata.org).await?;

    let id = random_id("skl");
    let insert = sqlx::query(
        "INSERT INTO skills
            (id, org_id, name, description, visibility, team_id, owner_user_id)
         VALUES
            ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (org_id, name) DO NOTHING
         RETURNING id",
    )
    .bind(&id)
    .bind(org_id)
    .bind(&metadata.name)
    .bind(&metadata.description)
    .bind(metadata.visibility.as_str())
    .bind(team_id)
    .bind(&user.id)
    .fetch_optional(&mut **db)
    .await
    .map_err(map_sql)?;
    if insert.is_some() {
        return Ok(id);
    }

    // A concurrent push won the race; re-evaluate the existing row.
    lookup_existing_skill(db, user, role, org_id, metadata)
        .await?
        .ok_or_else(ServerError::internal_error)
}

async fn lock_skill_push(
    db: &mut crate::db::DbTransaction<'_>,
    org_id: &str,
    skill_name: &str,
) -> Result<(), ServerError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2))")
        .bind(org_id)
        .bind(skill_name)
        .execute(&mut **db)
        .await
        .map_err(map_sql)?;
    Ok(())
}

async fn enforce_new_skill_quotas(
    db: &mut crate::db::DbTransaction<'_>,
    quotas: &QuotaConfig,
    org_id: &str,
    owner_user_id: &str,
    team_id: Option<&str>,
    org_slug: &str,
) -> Result<(), ServerError> {
    let counts = sqlx::query(
        "SELECT COUNT(*) AS org_count,
                COUNT(*) FILTER (WHERE owner_user_id = $2) AS owner_count,
                COUNT(*) FILTER (WHERE team_id = $3) AS team_count
         FROM skills
         WHERE org_id = $1",
    )
    .bind(org_id)
    .bind(owner_user_id)
    .bind(team_id)
    .fetch_one(&mut **db)
    .await
    .map_err(map_sql)?;
    let org_count: i64 = counts.get("org_count");
    if org_count >= quotas.max_skills_per_org {
        return Err(ServerError::quota_exceeded(format!(
            "org `{org_slug}` has reached the skill limit of {}",
            quotas.max_skills_per_org
        )));
    }
    let owner_count: i64 = counts.get("owner_count");
    if owner_count >= quotas.max_skills_per_owner_per_org {
        return Err(ServerError::quota_exceeded(format!(
            "owner has reached the per-owner skill limit of {} in org `{org_slug}`",
            quotas.max_skills_per_owner_per_org
        )));
    }
    if team_id.is_some() {
        let team_count: i64 = counts.get("team_count");
        if team_count >= quotas.max_team_skills_per_team {
            return Err(ServerError::quota_exceeded(format!(
                "team has reached the team-visible skill limit of {}",
                quotas.max_team_skills_per_team
            )));
        }
    }
    Ok(())
}

async fn lookup_existing_skill(
    db: &mut crate::db::DbTransaction<'_>,
    user: &AuthenticatedUser,
    role: AccessRole,
    org_id: &str,
    metadata: &PushMetadata,
) -> Result<Option<String>, ServerError> {
    let Some(row) = sqlx::query(
        "SELECT skills.id, skills.visibility, teams.slug AS team_slug,
                skills.owner_user_id,
                (SELECT team_memberships.role
                 FROM team_memberships
                 WHERE team_memberships.team_id = skills.team_id
                   AND team_memberships.user_id = $1) AS team_role
         FROM skills
         LEFT JOIN teams ON teams.id = skills.team_id
         WHERE skills.org_id = $2 AND skills.name = $3",
    )
    .bind(&user.id)
    .bind(org_id)
    .bind(&metadata.name)
    .fetch_optional(&mut **db)
    .await
    .map_err(map_sql)?
    else {
        return Ok(None);
    };

    let stored_visibility = visibility_from_db(row.get::<String, _>("visibility").as_str())?;
    let owner_user_id: Option<String> = row.get("owner_user_id");
    let team_role: Option<String> = row.get("team_role");
    if !can_read_visibility(
        user,
        role,
        stored_visibility,
        owner_user_id.as_deref(),
        team_role.is_some(),
    ) {
        return Err(ServerError::forbidden("permission denied"));
    }
    if !can_publish_visibility(role, stored_visibility, team_role.as_deref()) {
        return Err(forbidden_publish(role));
    }
    if stored_visibility != metadata.visibility {
        return Err(ServerError::visibility_mismatch(format!(
            "skill `{}/{}` already has scope `{}`; run `agentstack skill visibility set {}/{} --scope {}` or contact an admin to change scope",
            metadata.org,
            metadata.name,
            stored_visibility.as_str(),
            metadata.org,
            metadata.name,
            stored_visibility.as_str()
        )));
    }
    let stored_team: Option<String> = row.get("team_slug");
    if stored_team.as_deref() != metadata.team.as_deref() {
        return Err(ServerError::visibility_mismatch(format!(
            "skill `{}/{}` already has team `{}`; re-push with --team {}, or contact an admin to change team",
            metadata.org,
            metadata.name,
            stored_team.as_deref().unwrap_or("none"),
            stored_team.as_deref().unwrap_or("none")
        )));
    }
    Ok(Some(row.get("id")))
}

async fn next_version(
    db: &mut crate::db::DbTransaction<'_>,
    quotas: &QuotaConfig,
    skill_id: &str,
) -> Result<String, ServerError> {
    let row = sqlx::query(
        "SELECT next_version_number,
                (
                    SELECT COUNT(*)
                    FROM skill_versions
                    WHERE skill_versions.skill_id = skills.id
                ) AS version_count
         FROM skills
         WHERE id = $1
         FOR UPDATE",
    )
    .bind(skill_id)
    .fetch_one(&mut **db)
    .await
    .map_err(map_sql)?;
    let next: i64 = row.get("next_version_number");
    let count: i64 = row.get("version_count");
    if count >= quotas.max_versions_per_skill {
        return Err(ServerError::quota_exceeded(format!(
            "skill has reached the version limit of {}",
            quotas.max_versions_per_skill
        )));
    }
    Ok(next.to_string())
}

async fn insert_skill_version(
    db: &mut crate::db::DbTransaction<'_>,
    skill_id: &str,
    version: &str,
    archive_id: &str,
    metadata: &PushMetadata,
    user_id: &str,
) -> Result<(), ServerError> {
    let version_number = version
        .parse::<i64>()
        .map_err(|_| ServerError::internal_error())?;
    let version_id = random_id("ver");
    sqlx::query(
        "INSERT INTO skill_versions
            (id, skill_id, version_number, archive_id, description, published_by_user_id, status)
         VALUES
            ($1, $2, $3, $4, $5, $6, 'candidate')",
    )
    .bind(&version_id)
    .bind(skill_id)
    .bind(version_number)
    .bind(archive_id)
    .bind(&metadata.description)
    .bind(user_id)
    .execute(&mut **db)
    .await
    .map_err(map_sql)?;
    if !metadata.platform_tags.is_empty() {
        sqlx::query(
            "INSERT INTO skill_version_platform_tags (skill_version_id, tag)
             SELECT $1, tags.tag
             FROM unnest($2::text[]) AS tags(tag)",
        )
        .bind(&version_id)
        .bind(&metadata.platform_tags)
        .execute(&mut **db)
        .await
        .map_err(map_sql)?;
    }
    Ok(())
}

async fn touch_skill_updated_at(
    db: &mut crate::db::DbTransaction<'_>,
    skill_id: &str,
) -> Result<(), ServerError> {
    // Candidate pushes bump the skill row's updated_at but must not overwrite the
    // reader-visible description; that change is deferred to approve_skill_version.
    sqlx::query(
        "UPDATE skills
         SET updated_at = now()
         WHERE id = $1",
    )
    .bind(skill_id)
    .execute(&mut **db)
    .await
    .map_err(map_sql)?;
    Ok(())
}

async fn insert_audit_log(
    db: &mut crate::db::DbTransaction<'_>,
    org_id: &str,
    user_id: &str,
    resource_type: &'static str,
    skill_id: &str,
    action: &'static str,
    metadata: serde_json::Value,
) -> Result<String, ServerError> {
    let id = random_id("aud");
    sqlx::query(
        "INSERT INTO audit_log
            (id, org_id, actor_user_id, actor_principal_id, actor_type, action,
             resource_type, resource_id, metadata)
         VALUES
            ($1, $2, $3,
             (SELECT principal_id FROM human_profiles WHERE user_id = $3),
             'human', $4, $5, $6, $7::jsonb)",
    )
    .bind(&id)
    .bind(org_id)
    .bind(user_id)
    .bind(action)
    .bind(resource_type)
    .bind(skill_id)
    .bind(metadata.to_string())
    .execute(&mut **db)
    .await
    .map_err(map_audit_sql)?;
    tracing::info!(
        audit_event_id = %id,
        org_id = %org_id,
        actor_user_id = %user_id,
        resource_type = %resource_type,
        resource_id = %skill_id,
        action = %action,
        "audit_event_recorded"
    );
    Ok(id)
}

async fn load_visible_skill_summary(
    db: &DbPool,
    user: &AuthenticatedUser,
    org: &str,
    skill: &str,
) -> Result<RemoteSkill, ServerError> {
    visible_skill_summary(db, user, org, skill)
        .await?
        .ok_or_else(|| ServerError::skill_not_found(format!("no such skill `{org}/{skill}`")))
}

async fn load_yanked_visible_skill_summary(
    db: &DbPool,
    org: &str,
    skill: &str,
    versions: &[VersionInfo],
) -> Result<RemoteSkill, ServerError> {
    let Some(latest) = versions.first() else {
        return Err(ServerError::skill_not_found(format!(
            "no such skill `{org}/{skill}`"
        )));
    };
    let row = sqlx::query(
        "SELECT orgs.slug AS org_slug, skills.name, skills.description,
                skills.visibility, teams.slug AS team_slug,
                to_char(skills.updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS updated_at
         FROM skills
         JOIN orgs ON orgs.id = skills.org_id
         LEFT JOIN teams ON teams.id = skills.team_id
         WHERE orgs.slug = $1 AND skills.name = $2",
    )
    .bind(org)
    .bind(skill)
    .fetch_optional(db)
    .await
    .map_err(map_sql)?
    .ok_or_else(|| ServerError::skill_not_found(format!("no such skill `{org}/{skill}`")))?;

    Ok(RemoteSkill {
        org: row.get("org_slug"),
        name: row.get("name"),
        owner_email: None,
        latest_version: latest.version.clone(),
        current_version: versions
            .iter()
            .find(|version| version.current)
            .map(|version| version.version.clone()),
        description: row.get("description"),
        visibility: visibility_from_db(row.get::<String, _>("visibility").as_str())?,
        team: row.get("team_slug"),
        updated_at: row.get("updated_at"),
        platform_tags: latest.platform_tags.clone(),
    })
}

async fn resource_id_by_slug(
    db: &DbPool,
    org: &str,
    resource_type: &str,
    slug: &str,
) -> Result<Option<String>, ServerError> {
    let query = match resource_type {
        "skill" => {
            "SELECT skills.id
             FROM skills
             JOIN orgs ON orgs.id = skills.org_id
             WHERE orgs.slug = $1 AND skills.name = $2"
        }
        "stack" => {
            "SELECT stacks.id
             FROM stacks
             JOIN orgs ON orgs.id = stacks.org_id
             WHERE orgs.slug = $1 AND stacks.slug = $2"
        }
        "team" => {
            "SELECT teams.id
             FROM teams
             JOIN orgs ON orgs.id = teams.org_id
             WHERE orgs.slug = $1 AND teams.slug = $2"
        }
        _ => return Err(ServerError::internal_error()),
    };
    sqlx::query_scalar(query)
        .bind(org)
        .bind(slug)
        .fetch_optional(db)
        .await
        .map_err(map_sql)
}

async fn org_id_by_slug(db: &DbPool, org: &str) -> Result<Option<String>, ServerError> {
    let row = sqlx::query("SELECT id FROM orgs WHERE slug = $1")
        .bind(org)
        .fetch_optional(db)
        .await
        .map_err(map_sql)?;
    Ok(row.map(|row| row.get("id")))
}

fn map_audit_sql(err: sqlx::Error) -> ServerError {
    tracing::error!(error = %err, "audit log insert failed");
    audit_failed()
}

fn audit_failed() -> ServerError {
    ServerError::audit_failed("audit log insert failed; mutation rolled back")
}

fn permission_denied(denied: PermissionDenied) -> ServerError {
    let actual = denied
        .actual
        .map(AccessRole::as_str)
        .unwrap_or("no role in this org");
    ServerError::forbidden(format!(
        "permission denied: this action requires {} but your role is {}",
        denied.required.as_str(),
        actual
    ))
}

/// 403 for skill-management actions (approve, yank/deprecate, visibility) that
/// are gated by `can_manage_skill` rather than a flat role minimum, so the
/// message names the caller's role and the management requirement.
fn forbidden_manage(role: AccessRole) -> ServerError {
    ServerError::forbidden(format!(
        "permission denied: managing this resource requires org admin (or team admin for team-scoped resources) but your role is {}",
        role.as_str()
    ))
}

/// 403 for publish/visibility actions gated by `can_publish_visibility`, so the
/// message names the caller's role and the publish requirement.
fn forbidden_publish(role: AccessRole) -> ServerError {
    ServerError::forbidden(format!(
        "permission denied: publishing this resource requires publisher (or team admin for team-scoped resources) but your role is {}",
        role.as_str()
    ))
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex_encode(&Sha256::digest(bytes))
}

fn storage_key_for_hash(hash: &str) -> String {
    format!("sha256/{}/{}.tar.gz", &hash[..2], hash)
}

fn random_id(prefix: &str) -> String {
    let mut bytes = [0_u8; 16];
    OsRng.fill_bytes(&mut bytes);
    format!("{prefix}_{}", hex_encode(&bytes))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
