use anyhow::{Context, bail};
use rand::{RngCore, rngs::OsRng};
use sqlx::Row;

use crate::{auth::hash_token, config::QuotaConfig, db::DbPool};

/// Lifetime applied when issuing a bearer token.
///
/// Beta defaults to a finite 30-day window so leaked tokens self-expire. Use
/// [`TokenExpiry::Indefinite`] only for local/admin scenarios where the
/// operator has accepted the long-lived-credential risk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TokenExpiry {
    Days(u32),
    Indefinite,
}

impl TokenExpiry {
    pub const DEFAULT_TTL_DAYS: u32 = 30;
}

impl Default for TokenExpiry {
    fn default() -> Self {
        Self::Days(Self::DEFAULT_TTL_DAYS)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    ServerAdmin,
    OrgAdmin,
    Publisher,
    Reader,
}

impl Role {
    pub const fn as_str(self) -> &'static str {
        match self {
            Role::ServerAdmin => "server_admin",
            Role::OrgAdmin => "org_admin",
            Role::Publisher => "publisher",
            Role::Reader => "reader",
        }
    }
}

pub struct IssuedToken {
    pub raw_token: String,
    pub token_id: String,
    pub user_email: String,
    pub label: String,
}

#[derive(Debug)]
pub struct TokenRecord {
    pub id: String,
    pub user_email: String,
    pub label: String,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub last_used_at: Option<String>,
    pub revoked_at: Option<String>,
}

#[derive(Debug)]
pub struct UserRecord {
    pub id: String,
    pub email: String,
    pub name: Option<String>,
}

#[derive(Debug)]
pub struct UserListRecord {
    pub id: String,
    pub email: String,
    pub name: Option<String>,
    pub is_server_admin: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug)]
pub struct OrgRecord {
    pub id: String,
    pub slug: String,
    pub name: String,
}

#[derive(Debug)]
pub struct GrantRecord {
    pub org_slug: String,
    pub user_email: String,
    pub role: Role,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TeamRole {
    Member,
    TeamAdmin,
}

impl TeamRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            TeamRole::Member => "member",
            TeamRole::TeamAdmin => "team_admin",
        }
    }

    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "member" => Ok(TeamRole::Member),
            "team_admin" | "lead" => Ok(TeamRole::TeamAdmin),
            other => bail!(
                "unknown team role `{other}` (expected one of: member, team_admin; legacy lead is accepted)"
            ),
        }
    }
}

fn is_team_admin_role(role: &str) -> bool {
    matches!(role, "team_admin" | "lead")
}

#[derive(Debug)]
pub struct TeamRecord {
    pub org_slug: String,
    pub slug: String,
}

#[derive(Debug)]
pub struct TeamMemberRecord {
    pub email: String,
    pub role: TeamRole,
}

pub async fn create_team(
    db: &DbPool,
    org_slug: &str,
    team_slug: &str,
    team_admin_email: &str,
) -> anyhow::Result<TeamRecord> {
    create_team_with_quotas(
        db,
        &QuotaConfig::default(),
        org_slug,
        team_slug,
        team_admin_email,
    )
    .await
}

pub async fn create_team_with_quotas(
    db: &DbPool,
    quotas: &QuotaConfig,
    org_slug: &str,
    team_slug: &str,
    team_admin_email: &str,
) -> anyhow::Result<TeamRecord> {
    validate_slug(org_slug)?;
    validate_slug(team_slug)?;
    let team_admin_email = normalize_email(team_admin_email)?;
    let team_admin_user_id = id_by_field(db, "users", "email", &team_admin_email)
        .await?
        .with_context(|| format!("unknown user `{team_admin_email}`"))?;

    let mut tx = db.begin().await?;
    let org_id: Option<String> =
        sqlx::query_scalar("SELECT id FROM orgs WHERE slug = $1 FOR UPDATE")
            .bind(org_slug)
            .fetch_optional(&mut *tx)
            .await?;
    let org_id = org_id.with_context(|| format!("unknown org `{org_slug}`"))?;

    let membership: Option<String> =
        sqlx::query_scalar("SELECT user_id FROM org_members WHERE org_id = $1 AND user_id = $2")
            .bind(&org_id)
            .bind(&team_admin_user_id)
            .fetch_optional(&mut *tx)
            .await?;
    if membership.is_none() {
        bail!("user `{team_admin_email}` is not a member of org `{org_slug}`");
    }

    let exists: Option<String> =
        sqlx::query_scalar("SELECT id FROM teams WHERE org_id = $1 AND slug = $2")
            .bind(&org_id)
            .bind(team_slug)
            .fetch_optional(&mut *tx)
            .await?;
    if exists.is_some() {
        bail!("team `{org_slug}/{team_slug}` already exists");
    }

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM teams WHERE org_id = $1")
        .bind(&org_id)
        .fetch_one(&mut *tx)
        .await?;
    if count >= quotas.max_teams_per_org {
        bail!(
            "org `{org_slug}` has reached the team limit of {}",
            quotas.max_teams_per_org
        );
    }

    let id = random_id("tm");
    sqlx::query(
        "INSERT INTO teams (id, org_id, slug, name)
         VALUES ($1, $2, $3, $3)",
    )
    .bind(&id)
    .bind(&org_id)
    .bind(team_slug)
    .execute(&mut *tx)
    .await
    .with_context(|| format!("failed to create team `{org_slug}/{team_slug}`"))?;

    sqlx::query(
        "INSERT INTO team_memberships (team_id, org_id, user_id, role, created_at, updated_at)
         VALUES ($1, $2, $3, 'team_admin', now(), now())",
    )
    .bind(&id)
    .bind(&org_id)
    .bind(&team_admin_user_id)
    .execute(&mut *tx)
    .await
    .with_context(|| {
        format!("failed to add `{team_admin_email}` to team `{org_slug}/{team_slug}` as team_admin")
    })?;

    tx.commit().await?;

    Ok(TeamRecord {
        org_slug: org_slug.to_string(),
        slug: team_slug.to_string(),
    })
}

pub async fn list_teams(db: &DbPool, org_slug: &str) -> anyhow::Result<Vec<TeamRecord>> {
    validate_slug(org_slug)?;
    let org_id = id_by_field(db, "orgs", "slug", org_slug)
        .await?
        .with_context(|| format!("unknown org `{org_slug}`"))?;

    let rows = sqlx::query("SELECT slug FROM teams WHERE org_id = $1 ORDER BY slug ASC")
        .bind(&org_id)
        .fetch_all(db)
        .await
        .context("failed to list teams")?;

    Ok(rows
        .into_iter()
        .map(|row| TeamRecord {
            org_slug: org_slug.to_string(),
            slug: row.get("slug"),
        })
        .collect())
}

async fn team_id_by_slug(
    db: &DbPool,
    org_id: &str,
    team_slug: &str,
) -> anyhow::Result<Option<String>> {
    let row = sqlx::query("SELECT id FROM teams WHERE org_id = $1 AND slug = $2")
        .bind(org_id)
        .bind(team_slug)
        .fetch_optional(db)
        .await?;
    Ok(row.map(|r| r.get("id")))
}

pub async fn list_team_members(
    db: &DbPool,
    org_slug: &str,
    team_slug: &str,
) -> anyhow::Result<Vec<TeamMemberRecord>> {
    validate_slug(org_slug)?;
    validate_slug(team_slug)?;
    let org_id = id_by_field(db, "orgs", "slug", org_slug)
        .await?
        .with_context(|| format!("unknown org `{org_slug}`"))?;
    let team_id = team_id_by_slug(db, &org_id, team_slug)
        .await?
        .with_context(|| format!("unknown team `{org_slug}/{team_slug}`"))?;

    let rows = sqlx::query(
        "SELECT users.email AS email, team_memberships.role AS role
         FROM team_memberships
         JOIN users ON users.id = team_memberships.user_id
         WHERE team_memberships.team_id = $1
         ORDER BY users.email ASC",
    )
    .bind(&team_id)
    .fetch_all(db)
    .await
    .context("failed to list team members")?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let role: String = row.get("role");
        out.push(TeamMemberRecord {
            email: row.get("email"),
            role: TeamRole::parse(&role)?,
        });
    }
    Ok(out)
}

pub async fn add_team_member(
    db: &DbPool,
    org_slug: &str,
    team_slug: &str,
    user_email: &str,
    role: TeamRole,
) -> anyhow::Result<TeamMemberRecord> {
    add_team_member_with_quotas(
        db,
        &QuotaConfig::default(),
        org_slug,
        team_slug,
        user_email,
        role,
    )
    .await
}

pub async fn add_team_member_with_quotas(
    db: &DbPool,
    quotas: &QuotaConfig,
    org_slug: &str,
    team_slug: &str,
    user_email: &str,
    role: TeamRole,
) -> anyhow::Result<TeamMemberRecord> {
    validate_slug(org_slug)?;
    validate_slug(team_slug)?;
    let user_email = normalize_email(user_email)?;
    let user_id = id_by_field(db, "users", "email", &user_email)
        .await?
        .with_context(|| format!("unknown user `{user_email}`"))?;

    let mut tx = db.begin().await?;
    let org_id: Option<String> =
        sqlx::query_scalar("SELECT id FROM orgs WHERE slug = $1 FOR UPDATE")
            .bind(org_slug)
            .fetch_optional(&mut *tx)
            .await?;
    let org_id = org_id.with_context(|| format!("unknown org `{org_slug}`"))?;
    let team_id: Option<String> =
        sqlx::query_scalar("SELECT id FROM teams WHERE org_id = $1 AND slug = $2 FOR UPDATE")
            .bind(&org_id)
            .bind(team_slug)
            .fetch_optional(&mut *tx)
            .await?;
    let team_id = team_id.with_context(|| format!("unknown team `{org_slug}/{team_slug}`"))?;

    let is_member: Option<String> =
        sqlx::query_scalar("SELECT user_id FROM org_members WHERE org_id = $1 AND user_id = $2")
            .bind(&org_id)
            .bind(&user_id)
            .fetch_optional(&mut *tx)
            .await?;
    if is_member.is_none() {
        bail!("user `{user_email}` is not a member of org `{org_slug}`");
    }

    let existing_role = team_member_role(&mut tx, &team_id, &user_id).await?;
    if existing_role.is_none() {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM team_memberships WHERE team_id = $1")
                .bind(&team_id)
                .fetch_one(&mut *tx)
                .await?;
        if count >= quotas.max_team_members_per_team {
            bail!(
                "team `{org_slug}/{team_slug}` has reached the member limit of {}",
                quotas.max_team_members_per_team
            );
        }
    } else if existing_role.as_deref().is_some_and(is_team_admin_role)
        && role != TeamRole::TeamAdmin
    {
        ensure_team_keeps_admin(&mut tx, &team_id, org_slug, team_slug).await?;
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
    .bind(role.as_str())
    .execute(&mut *tx)
    .await
    .with_context(|| {
        format!(
            "failed to add `{user_email}` to team `{org_slug}/{team_slug}` as {}",
            role.as_str()
        )
    })?;
    tx.commit().await?;

    Ok(TeamMemberRecord {
        email: user_email,
        role,
    })
}

pub async fn set_team_role(
    db: &DbPool,
    org_slug: &str,
    team_slug: &str,
    user_email: &str,
    role: TeamRole,
) -> anyhow::Result<TeamMemberRecord> {
    validate_slug(org_slug)?;
    validate_slug(team_slug)?;
    let user_email = normalize_email(user_email)?;
    let org_id = id_by_field(db, "orgs", "slug", org_slug)
        .await?
        .with_context(|| format!("unknown org `{org_slug}`"))?;
    let team_id = team_id_by_slug(db, &org_id, team_slug)
        .await?
        .with_context(|| format!("unknown team `{org_slug}/{team_slug}`"))?;
    let user_id = id_by_field(db, "users", "email", &user_email)
        .await?
        .with_context(|| format!("unknown user `{user_email}`"))?;

    let mut tx = db.begin().await?;
    sqlx::query("SELECT id FROM teams WHERE id = $1 FOR UPDATE")
        .bind(&team_id)
        .fetch_one(&mut *tx)
        .await?;
    let Some(existing_role) = team_member_role(&mut tx, &team_id, &user_id).await? else {
        bail!("user `{user_email}` is not a member of team `{org_slug}/{team_slug}`");
    };
    if is_team_admin_role(&existing_role) && role != TeamRole::TeamAdmin {
        ensure_team_keeps_admin(&mut tx, &team_id, org_slug, team_slug).await?;
    }

    sqlx::query(
        "UPDATE team_memberships
         SET role = $1, updated_at = now()
         WHERE team_id = $2 AND user_id = $3",
    )
    .bind(role.as_str())
    .bind(&team_id)
    .bind(&user_id)
    .execute(&mut *tx)
    .await
    .with_context(|| {
        format!("failed to set role for `{user_email}` on team `{org_slug}/{team_slug}`")
    })?;
    tx.commit().await?;

    Ok(TeamMemberRecord {
        email: user_email,
        role,
    })
}

pub async fn remove_team_member(
    db: &DbPool,
    org_slug: &str,
    team_slug: &str,
    user_email: &str,
) -> anyhow::Result<()> {
    validate_slug(org_slug)?;
    validate_slug(team_slug)?;
    let user_email = normalize_email(user_email)?;
    let org_id = id_by_field(db, "orgs", "slug", org_slug)
        .await?
        .with_context(|| format!("unknown org `{org_slug}`"))?;
    let team_id = team_id_by_slug(db, &org_id, team_slug)
        .await?
        .with_context(|| format!("unknown team `{org_slug}/{team_slug}`"))?;
    let user_id = id_by_field(db, "users", "email", &user_email)
        .await?
        .with_context(|| format!("unknown user `{user_email}`"))?;

    let mut tx = db.begin().await?;
    sqlx::query("SELECT id FROM teams WHERE id = $1 FOR UPDATE")
        .bind(&team_id)
        .fetch_one(&mut *tx)
        .await?;
    let Some(existing_role) = team_member_role(&mut tx, &team_id, &user_id).await? else {
        bail!("user `{user_email}` is not a member of team `{org_slug}/{team_slug}`");
    };
    if is_team_admin_role(&existing_role) {
        ensure_team_keeps_admin(&mut tx, &team_id, org_slug, team_slug).await?;
    }

    sqlx::query("DELETE FROM team_memberships WHERE team_id = $1 AND user_id = $2")
        .bind(&team_id)
        .bind(&user_id)
        .execute(&mut *tx)
        .await
        .with_context(|| {
            format!("failed to remove `{user_email}` from team `{org_slug}/{team_slug}`")
        })?;
    tx.commit().await?;

    Ok(())
}

async fn ensure_team_keeps_admin(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    team_id: &str,
    org_slug: &str,
    team_slug: &str,
) -> anyhow::Result<()> {
    let admin_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM team_memberships WHERE team_id = $1 AND role IN ('team_admin', 'lead')",
    )
    .bind(team_id)
    .fetch_one(&mut **tx)
    .await?;
    if admin_count <= 1 {
        bail!("team `{org_slug}/{team_slug}` must have at least one team_admin");
    }
    Ok(())
}

pub async fn create_user(
    db: &DbPool,
    email: &str,
    name: Option<&str>,
) -> anyhow::Result<UserRecord> {
    let email = normalize_email(email)?;
    let name = normalize_optional(name);
    let id = random_id("usr");
    let principal_id = random_id("prn");
    let display_name = name.clone().unwrap_or_else(|| email.clone());
    if id_by_field(db, "users", "email", &email).await?.is_some() {
        bail!("user `{email}` already exists");
    }

    let mut tx = db.begin().await?;
    sqlx::query(
        "INSERT INTO users (id, email, name, created_at, updated_at)
         VALUES ($1, $2, $3, now(), now())",
    )
    .bind(&id)
    .bind(&email)
    .bind(&name)
    .execute(&mut *tx)
    .await
    .with_context(|| format!("failed to create user `{email}`"))?;

    sqlx::query(
        "INSERT INTO principals
            (id, principal_type, display_name, is_server_admin, created_at, updated_at)
         VALUES ($1, 'human', $2, false, now(), now())",
    )
    .bind(&principal_id)
    .bind(&display_name)
    .execute(&mut *tx)
    .await
    .with_context(|| format!("failed to create principal for `{email}`"))?;

    sqlx::query(
        "INSERT INTO human_profiles
            (principal_id, user_id, email, name, created_at, updated_at)
         VALUES ($1, $2, $3, $4, now(), now())",
    )
    .bind(&principal_id)
    .bind(&id)
    .bind(&email)
    .bind(&name)
    .execute(&mut *tx)
    .await
    .with_context(|| format!("failed to create human profile for `{email}`"))?;
    tx.commit().await?;

    Ok(UserRecord { id, email, name })
}

pub async fn create_org(db: &DbPool, slug: &str, name: Option<&str>) -> anyhow::Result<OrgRecord> {
    validate_slug(slug)?;
    let name = normalize_optional(name).unwrap_or_else(|| slug.to_string());
    let id = random_id("org");
    if id_by_field(db, "orgs", "slug", slug).await?.is_some() {
        bail!("org `{slug}` already exists");
    }

    sqlx::query(
        "INSERT INTO orgs (id, slug, name, created_at, updated_at)
         VALUES ($1, $2, $3, now(), now())",
    )
    .bind(&id)
    .bind(slug)
    .bind(&name)
    .execute(db)
    .await
    .with_context(|| format!("failed to create org `{slug}`"))?;

    Ok(OrgRecord {
        id,
        slug: slug.to_string(),
        name,
    })
}

pub async fn grant_org_role(
    db: &DbPool,
    org_slug: &str,
    user_email: &str,
    role: Role,
) -> anyhow::Result<GrantRecord> {
    grant_org_role_with_quotas(db, &QuotaConfig::default(), org_slug, user_email, role).await
}

pub async fn grant_org_role_with_quotas(
    db: &DbPool,
    quotas: &QuotaConfig,
    org_slug: &str,
    user_email: &str,
    role: Role,
) -> anyhow::Result<GrantRecord> {
    if role == Role::ServerAdmin {
        bail!(
            "`server_admin` is not an org role; use `agentstack-server admin users set-admin <email>` to grant the global server-admin flag"
        );
    }
    validate_slug(org_slug)?;
    let user_email = normalize_email(user_email)?;
    let user_id = id_by_field(db, "users", "email", &user_email)
        .await?
        .with_context(|| format!("unknown user `{user_email}`"))?;

    let mut tx = db.begin().await?;
    let org_id: Option<String> =
        sqlx::query_scalar("SELECT id FROM orgs WHERE slug = $1 FOR UPDATE")
            .bind(org_slug)
            .fetch_optional(&mut *tx)
            .await?;
    let org_id = org_id.with_context(|| format!("unknown org `{org_slug}`"))?;

    let existing: Option<String> =
        sqlx::query_scalar("SELECT user_id FROM org_members WHERE org_id = $1 AND user_id = $2")
            .bind(&org_id)
            .bind(&user_id)
            .fetch_optional(&mut *tx)
            .await?;
    if existing.is_none() {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM org_members WHERE org_id = $1")
            .bind(&org_id)
            .fetch_one(&mut *tx)
            .await?;
        if count >= quotas.max_org_members_per_org {
            bail!(
                "org `{org_slug}` has reached the member limit of {}",
                quotas.max_org_members_per_org
            );
        }
    }

    sqlx::query(
        "INSERT INTO org_members (org_id, user_id, role, created_at, updated_at)
         VALUES ($1, $2, $3, now(), now())
         ON CONFLICT(org_id, user_id) DO UPDATE SET
             role = excluded.role,
             updated_at = excluded.updated_at",
    )
    .bind(&org_id)
    .bind(&user_id)
    .bind(role.as_str())
    .execute(&mut *tx)
    .await
    .with_context(|| {
        format!(
            "failed to grant `{}` to `{user_email}` in `{org_slug}`",
            role.as_str()
        )
    })?;
    tx.commit().await?;

    Ok(GrantRecord {
        org_slug: org_slug.to_string(),
        user_email,
        role,
    })
}

pub async fn set_server_admin(
    db: &DbPool,
    user_email: &str,
    enabled: bool,
) -> anyhow::Result<UserListRecord> {
    let user_email = normalize_email(user_email)?;
    let user_id = id_by_field(db, "users", "email", &user_email)
        .await?
        .with_context(|| format!("unknown user `{user_email}`"))?;

    let mut tx = db.begin().await?;
    sqlx::query(
        "UPDATE users
         SET is_server_admin = $1,
             updated_at = now()
         WHERE id = $2",
    )
    .bind(enabled)
    .bind(&user_id)
    .execute(&mut *tx)
    .await
    .with_context(|| format!("failed to update server-admin flag for `{user_email}`"))?;

    let principal_update = sqlx::query(
        "UPDATE principals
         SET is_server_admin = $1,
             updated_at = now()
         FROM human_profiles
         WHERE human_profiles.principal_id = principals.id
           AND human_profiles.user_id = $2",
    )
    .bind(enabled)
    .bind(&user_id)
    .execute(&mut *tx)
    .await
    .with_context(|| format!("failed to update principal server-admin flag for `{user_email}`"))?;
    if principal_update.rows_affected() != 1 {
        bail!(
            "user `{user_email}` is missing a human principal; initialize the fresh local schema before issuing privileges"
        );
    }

    let row = sqlx::query(
        "SELECT id, email, name, is_server_admin,
                created_at::text AS created_at, updated_at::text AS updated_at
         FROM users WHERE id = $1",
    )
    .bind(&user_id)
    .fetch_one(&mut *tx)
    .await
    .with_context(|| format!("failed to load user `{user_email}`"))?;

    let record = UserListRecord {
        id: row.get("id"),
        email: row.get("email"),
        name: row.get("name"),
        is_server_admin: row.get("is_server_admin"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    };
    tx.commit().await?;

    Ok(record)
}

pub async fn issue_token(
    db: &DbPool,
    user_email: &str,
    label: &str,
    expiry: TokenExpiry,
) -> anyhow::Result<IssuedToken> {
    issue_token_with_quotas(db, &QuotaConfig::default(), user_email, label, expiry).await
}

pub async fn issue_token_with_quotas(
    db: &DbPool,
    quotas: &QuotaConfig,
    user_email: &str,
    label: &str,
    expiry: TokenExpiry,
) -> anyhow::Result<IssuedToken> {
    let user_email = normalize_email(user_email)?;
    let label = normalize_label(label)?;
    let expiry_days: Option<i32> = match expiry {
        TokenExpiry::Indefinite => None,
        TokenExpiry::Days(0) => anyhow::bail!("token TTL must be at least 1 day"),
        TokenExpiry::Days(days) => Some(
            i32::try_from(days)
                .map_err(|_| anyhow::anyhow!("token TTL `{days}` days is too large"))?,
        ),
    };

    let mut tx = db.begin().await?;
    let row = sqlx::query(
        "SELECT users.id, human_profiles.principal_id
         FROM users
         LEFT JOIN human_profiles ON human_profiles.user_id = users.id
         WHERE users.email = $1
         FOR UPDATE OF users",
    )
    .bind(&user_email)
    .fetch_optional(&mut *tx)
    .await?;
    let row = row.with_context(|| format!("unknown user `{user_email}`"))?;
    let user_id: String = row.get("id");
    let principal_id: Option<String> = row.get("principal_id");
    let principal_id = principal_id.with_context(|| {
        format!(
            "user `{user_email}` is missing a human principal; initialize the fresh local schema before issuing tokens"
        )
    })?;

    let active: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tokens
         WHERE user_id = $1
           AND revoked_at IS NULL
           AND (expires_at IS NULL OR expires_at > now())",
    )
    .bind(&user_id)
    .fetch_one(&mut *tx)
    .await?;
    if active >= quotas.max_active_tokens_per_user {
        bail!(
            "user `{user_email}` has reached the active token limit of {}",
            quotas.max_active_tokens_per_user
        );
    }

    let raw_token = random_token();
    let token_hash = hash_token(&raw_token);
    let token_id = random_id("tok");

    sqlx::query(
        "INSERT INTO tokens
            (id, user_id, principal_id, label, token_hash, token_kind, scopes,
             created_by_principal_id, created_at, expires_at)
         VALUES ($1, $2, $3, $4, $5, 'user', '[\"registry:*\"]'::jsonb,
                 $3, now(),
                 CASE WHEN $6::int IS NULL THEN NULL
                      ELSE now() + make_interval(days => $6::int) END)",
    )
    .bind(&token_id)
    .bind(&user_id)
    .bind(&principal_id)
    .bind(&label)
    .bind(&token_hash)
    .bind(expiry_days)
    .execute(&mut *tx)
    .await
    .context("failed to issue token")?;
    tx.commit().await?;

    Ok(IssuedToken {
        raw_token,
        token_id,
        user_email,
        label,
    })
}

/// Shared SELECT/JOIN portion for token records; callers append WHERE/ORDER BY
/// and feed rows to `token_record_from_row`.
const TOKEN_SELECT: &str = "SELECT tokens.id, users.email AS user_email, tokens.label,
            tokens.created_at::text AS created_at,
            tokens.expires_at::text AS expires_at,
            tokens.last_used_at::text AS last_used_at,
            tokens.revoked_at::text AS revoked_at
     FROM tokens
     JOIN users ON users.id = tokens.user_id";

pub async fn list_tokens(db: &DbPool) -> anyhow::Result<Vec<TokenRecord>> {
    let rows = sqlx::query(&format!(
        "{TOKEN_SELECT} ORDER BY tokens.created_at DESC, tokens.id ASC"
    ))
    .fetch_all(db)
    .await
    .context("failed to list tokens")?;

    Ok(rows.into_iter().map(token_record_from_row).collect())
}

pub async fn revoke_token(db: &DbPool, token_id: &str) -> anyhow::Result<TokenRecord> {
    let token_id = token_id.trim();
    if token_id.is_empty() {
        bail!("token id must not be empty");
    }

    let Some(existing) = token_by_id(db, token_id).await? else {
        bail!("unknown token `{token_id}`");
    };
    if existing.revoked_at.is_some() {
        return Ok(existing);
    }

    sqlx::query(
        "UPDATE tokens
         SET revoked_at = now()
         WHERE id = $1",
    )
    .bind(token_id)
    .execute(db)
    .await
    .with_context(|| format!("failed to revoke token `{token_id}`"))?;

    token_by_id(db, token_id)
        .await?
        .with_context(|| format!("unknown token `{token_id}`"))
}

pub async fn list_users(db: &DbPool) -> anyhow::Result<Vec<UserListRecord>> {
    let rows = sqlx::query(
        "SELECT id, email, name, is_server_admin,
                created_at::text AS created_at, updated_at::text AS updated_at
         FROM users
         ORDER BY email ASC",
    )
    .fetch_all(db)
    .await
    .context("failed to list users")?;

    Ok(rows
        .into_iter()
        .map(|row| UserListRecord {
            id: row.get("id"),
            email: row.get("email"),
            name: row.get("name"),
            is_server_admin: row.get("is_server_admin"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        })
        .collect())
}

async fn team_member_role(
    tx: &mut crate::db::DbTransaction<'_>,
    team_id: &str,
    user_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT role FROM team_memberships WHERE team_id = $1 AND user_id = $2")
        .bind(team_id)
        .bind(user_id)
        .fetch_optional(&mut **tx)
        .await
}

async fn id_by_field(
    db: &DbPool,
    table: &str,
    field: &str,
    value: &str,
) -> Result<Option<String>, sqlx::Error> {
    let row = sqlx::query(&format!("SELECT id FROM {table} WHERE {field} = $1"))
        .bind(value)
        .fetch_optional(db)
        .await?;

    Ok(row.map(|row| row.get("id")))
}

async fn token_by_id(db: &DbPool, token_id: &str) -> anyhow::Result<Option<TokenRecord>> {
    let row = sqlx::query(&format!("{TOKEN_SELECT} WHERE tokens.id = $1"))
        .bind(token_id)
        .fetch_optional(db)
        .await
        .with_context(|| format!("failed to load token `{token_id}`"))?;

    Ok(row.map(token_record_from_row))
}

fn token_record_from_row(row: crate::db::DbRow) -> TokenRecord {
    TokenRecord {
        id: row.get("id"),
        user_email: row.get("user_email"),
        label: row.get("label"),
        created_at: row.get("created_at"),
        expires_at: row.get("expires_at"),
        last_used_at: row.get("last_used_at"),
        revoked_at: row.get("revoked_at"),
    }
}

pub(crate) fn random_id(prefix: &str) -> String {
    let mut bytes = [0_u8; 16];
    OsRng.fill_bytes(&mut bytes);
    format!("{prefix}_{}", hex_encode(&bytes))
}

fn random_token() -> String {
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    format!("adk_{}", hex_encode(&bytes))
}

pub(crate) fn normalize_email(email: &str) -> anyhow::Result<String> {
    let email = email.trim().to_ascii_lowercase();
    let parts: Vec<_> = email.split('@').collect();
    if parts.len() != 2
        || parts[0].is_empty()
        || parts[1].is_empty()
        || !parts[1].contains('.')
        || email.chars().any(char::is_whitespace)
    {
        bail!("email must be a non-empty email address without whitespace");
    }
    Ok(email)
}

pub(crate) fn normalize_optional(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn normalize_label(label: &str) -> anyhow::Result<String> {
    let label = label.trim();
    if label.is_empty() {
        bail!("--label must not be empty");
    }
    Ok(label.to_string())
}

fn validate_slug(slug: &str) -> anyhow::Result<()> {
    if slug.is_empty() {
        bail!("slug must not be empty");
    }
    let first = slug.chars().next().expect("checked non-empty");
    if !first.is_ascii_lowercase() {
        bail!("slug must start with a lowercase ASCII letter");
    }
    if slug.chars().count() > 64 {
        bail!("slug must be at most 64 characters");
    }
    if !slug
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        bail!("slug may only contain lowercase letters, digits, and hyphens");
    }
    if slug.contains("--") {
        bail!("slug must not contain consecutive hyphens");
    }
    if slug.ends_with('-') {
        bail!("slug must not end with a hyphen");
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_email_rejects_bad_shapes() {
        for email in ["", "@", "a@", "@b", "a@@b", "ab", "a@b"] {
            assert!(
                normalize_email(email).is_err(),
                "{email} should be rejected"
            );
        }
        assert_eq!(normalize_email("a@b.c").unwrap(), "a@b.c");
    }
}
