use sqlx::{Postgres, QueryBuilder, Row};

use crate::{
    auth::AuthenticatedUser,
    db::DbPool,
    error::{ServerError, map_sql},
    registry::{
        authz::{
            AccessRole, PermissionDenied, can_read_version, can_read_visibility,
            is_team_admin_role, require_role, role_for,
        },
        types::{
            CatalogSort, PackageHash, RemoteSkill, SkillMetadata, StoredMetadata, VersionInfo,
            VersionStatus, Visibility,
        },
        validate_slug,
    },
};

pub(crate) struct SkillCatalogFilters<'a> {
    pub(crate) org: Option<&'a str>,
    pub(crate) team: Option<&'a str>,
    pub(crate) query: Option<&'a str>,
    pub(crate) platforms: &'a [String],
    pub(crate) visibility: Option<Visibility>,
    pub(crate) owner: Option<&'a str>,
    pub(crate) sort: Option<CatalogSort>,
    pub(crate) limit: Option<usize>,
}

const CATALOG_PROJECTION_TEMPLATE: &str = "                latest_versions.version_number::text AS latest_version,
                latest_versions.description AS latest_description,
                to_char(latest_versions.yanked_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS latest_yanked_at,
                visible_versions.version_number::text AS visible_version,
                visible_versions.description AS visible_description,
                to_char(visible_versions.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS visible_updated_at,
                COALESCE((
                    SELECT jsonb_agg(tag ORDER BY tag)
                    FROM skill_version_platform_tags
                    WHERE skill_version_id = visible_versions.id
                ), '[]'::jsonb)::text AS visible_platform_tags_json,
                current_versions.version_number::text AS current_version,
                current_versions.description AS current_description,
                to_char(current_versions.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS current_updated_at,
                to_char(current_versions.yanked_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS current_yanked_at,
                to_char({updated_at_column} AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS updated_at,
                COALESCE((
                    SELECT jsonb_agg(tag ORDER BY tag)
                    FROM skill_version_platform_tags
                    WHERE skill_version_id = latest_versions.id
                ), '[]'::jsonb)::text AS platform_tags_json,
                COALESCE((
                    SELECT jsonb_agg(tag ORDER BY tag)
                    FROM skill_version_platform_tags
                    WHERE skill_version_id = current_versions.id
                ), '[]'::jsonb)::text AS current_platform_tags_json,
";

const CATALOG_VERSION_JOINS_TEMPLATE: &str = "         JOIN LATERAL (
             SELECT *
             FROM skill_versions
             WHERE skill_versions.skill_id = {skill_id_column}
             ORDER BY skill_versions.version_number DESC
             LIMIT 1
         ) AS latest_versions ON true
         LEFT JOIN LATERAL (
             SELECT *
             FROM skill_versions
             WHERE skill_versions.skill_id = {skill_id_column}
               AND skill_versions.yanked_at IS NULL
             ORDER BY skill_versions.version_number DESC
             LIMIT 1
         ) AS visible_versions ON true
         LEFT JOIN skill_versions AS current_versions
           ON current_versions.id = {current_version_id_column}";

fn catalog_projection_columns(updated_at_column: &str) -> String {
    CATALOG_PROJECTION_TEMPLATE.replace("{updated_at_column}", updated_at_column)
}

fn catalog_version_joins(skill_id_column: &str, current_version_id_column: &str) -> String {
    CATALOG_VERSION_JOINS_TEMPLATE
        .replace("{skill_id_column}", skill_id_column)
        .replace("{current_version_id_column}", current_version_id_column)
}

/// Escape LIKE/ILIKE metacharacters so a user query is matched literally.
///
/// Backslash is escaped first (it is the escape character), then `%` and `_`.
/// The resulting pattern is used with `ILIKE ... ESCAPE '\'`.
fn escape_like(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

pub(crate) async fn latest_skills_with_filters(
    db: &DbPool,
    user: &AuthenticatedUser,
    filters: SkillCatalogFilters<'_>,
) -> Result<Vec<RemoteSkill>, ServerError> {
    if let Some(org) = filters.org {
        validate_slug(org).map_err(ServerError::validation_error)?;
        require_role(user, org, AccessRole::Reader).map_err(permission_denied)?;
    }
    if let Some(team) = filters.team {
        validate_slug(team).map_err(ServerError::validation_error)?;
    }
    if filters.limit == Some(0) {
        return Ok(Vec::new());
    }

    let mut sql = QueryBuilder::<Postgres>::new(
        "WITH candidate_skills AS (
             SELECT orgs.slug AS org_slug, skills.id AS skill_id, skills.name,
                    skills.description, skills.visibility, teams.slug AS team_slug,
                    skills.updated_at, skills.owner_user_id,
                    owner_users.email AS owner_email, skills.current_version_id,
                    skills.team_id
             FROM skills
             JOIN orgs ON orgs.id = skills.org_id
             JOIN users AS owner_users ON owner_users.id = skills.owner_user_id
             LEFT JOIN teams ON teams.id = skills.team_id
             WHERE EXISTS (
                 SELECT 1
                 FROM skill_versions AS candidate_versions
                 WHERE candidate_versions.skill_id = skills.id
                   AND candidate_versions.yanked_at IS NULL
             )",
    );
    if let Some(org) = filters.org {
        sql.push(" AND orgs.slug = ");
        sql.push_bind(org);
    } else if !user.is_server_admin {
        sql.push(
            " AND EXISTS (
                  SELECT 1
                  FROM org_members
                  WHERE org_members.org_id = skills.org_id
                    AND org_members.user_id = ",
        );
        sql.push_bind(&user.id);
        sql.push(" AND org_members.role IN ('org_admin', 'publisher', 'reader'))");
    }
    if let Some(visibility) = filters.visibility {
        sql.push(" AND skills.visibility = ");
        sql.push_bind(visibility.as_str());
    }
    if let Some(team) = filters.team {
        sql.push(" AND teams.slug = ");
        sql.push_bind(team);
    }
    if let Some(owner) = filters.owner {
        sql.push(" AND owner_users.email = ");
        sql.push_bind(owner);
    }

    let q = filters
        .query
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(q) = q {
        let pattern = format!("%{}%", escape_like(q));
        sql.push(
            " AND (
                  orgs.slug ILIKE ",
        );
        sql.push_bind(pattern.clone());
        sql.push(" ESCAPE '\\' OR skills.name ILIKE ");
        sql.push_bind(pattern.clone());
        sql.push(" ESCAPE '\\' OR skills.description ILIKE ");
        sql.push_bind(pattern.clone());
        sql.push(
            " ESCAPE '\\' OR EXISTS (
                  SELECT 1
                  FROM skill_versions AS query_versions
                  WHERE query_versions.skill_id = skills.id
                    AND query_versions.description ILIKE ",
        );
        sql.push_bind(pattern);
        sql.push(" ESCAPE '\\'))");
    }
    if !filters.platforms.is_empty() {
        sql.push(
            " AND EXISTS (
                  SELECT 1
                  FROM skill_versions AS platform_versions
                  JOIN skill_version_platform_tags
                    ON skill_version_platform_tags.skill_version_id = platform_versions.id
                  WHERE platform_versions.skill_id = skills.id
                    AND (",
        );
        for (index, platform) in filters.platforms.iter().enumerate() {
            if index > 0 {
                sql.push(" OR ");
            }
            sql.push("skill_version_platform_tags.tag = ");
            sql.push_bind(platform);
        }
        sql.push("))");
    }
    sql.push(" ORDER BY orgs.slug ASC, skills.name ASC");
    if let Some(limit) = sql_candidate_limit(user, &filters) {
        sql.push(" LIMIT ");
        sql.push_bind(limit);
    }
    sql.push(
        ")
         SELECT candidate_skills.org_slug, candidate_skills.name,
                candidate_skills.description, candidate_skills.visibility,
                candidate_skills.team_slug,
",
    );
    sql.push(catalog_projection_columns("candidate_skills.updated_at"));
    sql.push(
        "                candidate_skills.owner_user_id,
                candidate_skills.owner_email,
                (SELECT team_memberships.role
                 FROM team_memberships
                 WHERE team_memberships.team_id = candidate_skills.team_id
                   AND team_memberships.user_id = ",
    );
    sql.push_bind(&user.id);
    sql.push(
        ") AS team_role
         FROM candidate_skills
",
    );
    sql.push(catalog_version_joins(
        "candidate_skills.skill_id",
        "candidate_skills.current_version_id",
    ));
    sql.push("\n         ORDER BY candidate_skills.org_slug ASC, candidate_skills.name ASC");

    let rows = sql.build().fetch_all(db).await.map_err(map_sql)?;

    // The SQL above already applies the visibility/team/owner filters; only the
    // per-user authz and the visible-field text match remain app-side.
    let q_lower = q.map(str::to_ascii_lowercase);
    let mut out = Vec::new();
    for row in rows {
        let org: String = row.get("org_slug");
        let visibility = visibility_from_db(row.get::<String, _>("visibility").as_str())?;
        let Some(role) = role_for(user, &org) else {
            continue;
        };
        if let Some(skill) =
            remote_skill_from_catalog_row(row, user, role, visibility, filters.platforms)?
        {
            if let Some(q) = &q_lower {
                let haystack = format!("{} {} {}", skill.org, skill.name, skill.description)
                    .to_ascii_lowercase();
                if !haystack.contains(q) {
                    continue;
                }
            }
            out.push(skill);
        }
    }

    sort_remote_skills(&mut out, filters.sort);
    Ok(out)
}

pub(crate) async fn visible_skill_summary(
    db: &DbPool,
    user: &AuthenticatedUser,
    org: &str,
    skill: &str,
) -> Result<Option<RemoteSkill>, ServerError> {
    validate_slug(org).map_err(ServerError::validation_error)?;
    validate_slug(skill).map_err(ServerError::validation_error)?;
    let role = require_role(user, org, AccessRole::Reader).map_err(permission_denied)?;
    let mut sql = String::from(
        "SELECT orgs.slug AS org_slug, skills.name,
                skills.description, skills.visibility,
                teams.slug AS team_slug,
",
    );
    sql.push_str(&catalog_projection_columns("skills.updated_at"));
    sql.push_str(
        "                skills.owner_user_id,
                owner_users.email AS owner_email,
                team_memberships.role AS team_role
         FROM skills
         JOIN orgs ON orgs.id = skills.org_id
         JOIN users AS owner_users ON owner_users.id = skills.owner_user_id
         LEFT JOIN teams ON teams.id = skills.team_id
         LEFT JOIN team_memberships
           ON team_memberships.team_id = skills.team_id
          AND team_memberships.user_id = $1
",
    );
    sql.push_str(&catalog_version_joins(
        "skills.id",
        "skills.current_version_id",
    ));
    sql.push_str(
        "
         WHERE orgs.slug = $2
           AND skills.name = $3
           AND EXISTS (
               SELECT 1
               FROM skill_versions AS candidate_versions
               WHERE candidate_versions.skill_id = skills.id
                 AND candidate_versions.yanked_at IS NULL
           )",
    );
    let row = sqlx::query(&sql)
        .bind(&user.id)
        .bind(org)
        .bind(skill)
        .fetch_optional(db)
        .await
        .map_err(map_sql)?;

    let Some(row) = row else {
        return Ok(None);
    };
    let visibility = visibility_from_db(row.get::<String, _>("visibility").as_str())?;
    remote_skill_from_catalog_row(row, user, role, visibility, &[])
}

fn remote_skill_from_catalog_row(
    row: crate::db::DbRow,
    user: &AuthenticatedUser,
    role: AccessRole,
    visibility: Visibility,
    platform_filters: &[String],
) -> Result<Option<RemoteSkill>, ServerError> {
    let org: String = row.get("org_slug");
    let team: Option<String> = row.get("team_slug");
    let owner_user_id: Option<String> = row.get("owner_user_id");
    let owner_email: Option<String> = row.get("owner_email");
    let team_role: Option<String> = row.get("team_role");
    if !can_read_visibility(
        user,
        role,
        visibility,
        owner_user_id.as_deref(),
        team_role.is_some(),
    ) {
        return Ok(None);
    }
    let current_version: Option<String> = row.get("current_version");
    let current_yanked_at: Option<String> = row.get("current_yanked_at");
    let latest_yanked_at: Option<String> = row.get("latest_yanked_at");
    let can_view_candidate_uploads = role >= AccessRole::Publisher
        || (visibility == Visibility::Team && is_team_admin_role(team_role.as_deref()));
    let reader_view = !can_view_candidate_uploads;
    let visible_current_version = if current_yanked_at.is_some() {
        None
    } else {
        current_version.clone()
    };
    let output_latest_version = if reader_view {
        let Some(current) = current_version.clone() else {
            return Ok(None);
        };
        if current_yanked_at.is_some() {
            return Ok(None);
        }
        current
    } else if latest_yanked_at.is_some() {
        let Some(visible) = row.get::<Option<String>, _>("visible_version") else {
            return Ok(None);
        };
        visible
    } else {
        row.get("latest_version")
    };
    let platform_tags = if reader_view {
        let raw: Option<String> = row.get("current_platform_tags_json");
        platform_tags_from_json(raw.as_deref().unwrap_or("[]"))?
    } else {
        let raw: Option<String> = if latest_yanked_at.is_some() {
            row.get("visible_platform_tags_json")
        } else {
            row.get("platform_tags_json")
        };
        platform_tags_from_json(raw.as_deref().unwrap_or("[]"))?
    };
    if !platform_filters.is_empty()
        && !platform_filters
            .iter()
            .any(|filter| platform_tags.iter().any(|tag| tag == filter))
    {
        return Ok(None);
    }

    let name: String = row.get("name");
    let description: String = if reader_view {
        row.get::<Option<String>, _>("current_description")
            .unwrap_or_else(|| row.get("description"))
    } else if latest_yanked_at.is_some() {
        row.get::<Option<String>, _>("visible_description")
            .unwrap_or_else(|| row.get("description"))
    } else {
        row.get("latest_description")
    };
    let updated_at = if reader_view {
        row.get::<Option<String>, _>("current_updated_at")
            .unwrap_or_else(|| row.get("updated_at"))
    } else if latest_yanked_at.is_some() {
        row.get::<Option<String>, _>("visible_updated_at")
            .unwrap_or_else(|| row.get("updated_at"))
    } else {
        row.get("updated_at")
    };

    Ok(Some(RemoteSkill {
        org,
        name,
        owner_email,
        latest_version: output_latest_version,
        current_version: visible_current_version,
        description,
        visibility,
        team,
        updated_at,
        platform_tags,
    }))
}

fn sql_candidate_limit(user: &AuthenticatedUser, filters: &SkillCatalogFilters<'_>) -> Option<i64> {
    let limit = filters.limit?;
    if !matches!(filters.sort, None | Some(CatalogSort::Name))
        || filters
            .query
            .map(str::trim)
            .is_some_and(|query| !query.is_empty())
        || !filters.platforms.is_empty()
    {
        return None;
    }
    if user.is_server_admin {
        return Some(limit as i64);
    }
    let org = filters.org?;
    let role = role_for(user, org)?;
    if role >= AccessRole::OrgAdmin {
        return Some(limit as i64);
    }
    None
}

fn sort_remote_skills(rows: &mut [RemoteSkill], sort: Option<CatalogSort>) {
    match sort.unwrap_or(CatalogSort::Name) {
        CatalogSort::Name => {
            rows.sort_by(|a, b| a.org.cmp(&b.org).then_with(|| a.name.cmp(&b.name)))
        }
        CatalogSort::Updated => rows.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.org.cmp(&b.org))
                .then_with(|| a.name.cmp(&b.name))
        }),
        CatalogSort::Owner => rows.sort_by(|a, b| {
            a.owner_email
                .cmp(&b.owner_email)
                .then_with(|| a.org.cmp(&b.org))
                .then_with(|| a.name.cmp(&b.name))
        }),
    }
}

pub(crate) async fn fetch_metadata(
    db: &DbPool,
    user: &AuthenticatedUser,
    org: &str,
    skill: &str,
    version: Option<&str>,
) -> Result<Option<StoredMetadata>, ServerError> {
    if version.is_none() {
        let Some(skill_row) = sqlx::query(
            "SELECT skills.id AS skill_id, skills.visibility, skills.current_version_id,
                teams.slug AS team_slug,
                skills.owner_user_id,
                owner_users.email AS owner_email,
                (SELECT team_memberships.role
                 FROM team_memberships
                 WHERE team_memberships.team_id = skills.team_id
                   AND team_memberships.user_id = $1) AS team_role
             FROM skills
             JOIN orgs ON orgs.id = skills.org_id
             JOIN users AS owner_users ON owner_users.id = skills.owner_user_id
             LEFT JOIN teams ON teams.id = skills.team_id
             WHERE orgs.slug = $2 AND skills.name = $3",
        )
        .bind(&user.id)
        .bind(org)
        .bind(skill)
        .fetch_optional(db)
        .await
        .map_err(map_sql)?
        else {
            return Ok(None);
        };

        let visibility = visibility_from_db(skill_row.get::<String, _>("visibility").as_str())?;
        let role =
            role_for(user, org).ok_or_else(|| ServerError::forbidden("permission denied"))?;
        let owner_user_id: Option<String> = skill_row.get("owner_user_id");
        let team_role: Option<String> = skill_row.get("team_role");
        if !can_read_visibility(
            user,
            role,
            visibility,
            owner_user_id.as_deref(),
            team_role.is_some(),
        ) {
            return Ok(None);
        }

        let Some(current_version_id) = skill_row.get::<Option<String>, _>("current_version_id")
        else {
            return Err(no_current_version_error(org, skill));
        };

        let row = sqlx::query(
            "SELECT orgs.slug AS org_slug, skills.name, skills.visibility,
                    teams.slug AS team_slug,
                    to_char(skills.updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS updated_at, skill_versions.id AS version_id,
                    skills.current_version_id, skill_versions.version_number::text AS version, skill_versions.status,
                    skill_versions.description,
                    to_char(skill_versions.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at,
                    COALESCE((
                        SELECT jsonb_agg(tag ORDER BY tag)
                        FROM skill_version_platform_tags
                        WHERE skill_version_id = skill_versions.id
                    ), '[]'::jsonb)::text AS platform_tags_json,
                    to_char(skill_versions.yanked_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS yanked_at,
                    skill_versions.yank_reason,
                    to_char(skill_versions.deprecated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS deprecated_at,
                    skill_versions.deprecation_reason,
                    $1 AS owner_user_id,
                    $2 AS owner_email,
                    $3 AS team_role,
                    archives.hash_algorithm, archives.hash_hex, archives.storage_key
             FROM skills
             JOIN orgs ON orgs.id = skills.org_id
             LEFT JOIN teams ON teams.id = skills.team_id
             JOIN skill_versions ON skill_versions.id = skills.current_version_id
             JOIN archives ON archives.id = skill_versions.archive_id
             WHERE orgs.slug = $4 AND skills.name = $5 AND skill_versions.id = $6",
        )
        .bind(owner_user_id)
        .bind(skill_row.get::<Option<String>, _>("owner_email"))
        .bind(team_role)
        .bind(org)
        .bind(skill)
        .bind(current_version_id)
        .fetch_optional(db)
        .await
        .map_err(map_sql)?
        .ok_or_else(ServerError::internal_error)?;

        return row_to_stored_metadata(row, user, org, role, visibility);
    }

    let version = version.expect("checked above");
    let version_number = parse_version_number(version)?;
    let row = sqlx::query(
        "SELECT orgs.slug AS org_slug, skills.name, skills.visibility,
                teams.slug AS team_slug,
                to_char(skills.updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS updated_at, skill_versions.id AS version_id,
                skills.current_version_id, skill_versions.version_number::text AS version, skill_versions.status,
                skill_versions.description,
                to_char(skill_versions.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at,
                COALESCE((
                    SELECT jsonb_agg(tag ORDER BY tag)
                    FROM skill_version_platform_tags
                    WHERE skill_version_id = skill_versions.id
                ), '[]'::jsonb)::text AS platform_tags_json,
                to_char(skill_versions.yanked_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS yanked_at,
                skill_versions.yank_reason,
                to_char(skill_versions.deprecated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS deprecated_at,
                skill_versions.deprecation_reason,
                skills.owner_user_id,
                owner_users.email AS owner_email,
                (SELECT team_memberships.role
                 FROM team_memberships
                 WHERE team_memberships.team_id = skills.team_id
                   AND team_memberships.user_id = $1) AS team_role,
                archives.hash_algorithm, archives.hash_hex, archives.storage_key
         FROM skills
         JOIN orgs ON orgs.id = skills.org_id
         JOIN users AS owner_users ON owner_users.id = skills.owner_user_id
         LEFT JOIN teams ON teams.id = skills.team_id
         JOIN skill_versions ON skill_versions.skill_id = skills.id
         JOIN archives ON archives.id = skill_versions.archive_id
         WHERE orgs.slug = $2 AND skills.name = $3 AND skill_versions.version_number = $4",
    )
    .bind(&user.id)
    .bind(org)
    .bind(skill)
    .bind(version_number)
    .fetch_optional(db)
    .await
    .map_err(map_sql)?;

    let Some(row) = row else {
        return Ok(None);
    };
    let visibility = visibility_from_db(row.get::<String, _>("visibility").as_str())?;
    let role = role_for(user, org).ok_or_else(|| ServerError::forbidden("permission denied"))?;
    row_to_stored_metadata(row, user, org, role, visibility)
}

fn row_to_stored_metadata(
    row: crate::db::DbRow,
    user: &AuthenticatedUser,
    org: &str,
    role: AccessRole,
    visibility: Visibility,
) -> Result<Option<StoredMetadata>, ServerError> {
    let owner_user_id: Option<String> = row.get("owner_user_id");
    let team_role: Option<String> = row.get("team_role");
    let status = version_status_from_db(row.get::<String, _>("status").as_str())?;
    if !can_read_version(
        user,
        role,
        visibility,
        owner_user_id.as_deref(),
        team_role.as_deref(),
        status,
    ) {
        return Ok(None);
    }
    let version_id: String = row.get("version_id");
    let current_version_id: Option<String> = row.get("current_version_id");
    let current = current_version_id.as_deref() == Some(version_id.as_str());
    if current && status != VersionStatus::Approved {
        tracing::error!(org, "current version is not approved");
        return Err(ServerError::internal_error());
    }
    Ok(Some(StoredMetadata {
        metadata: SkillMetadata {
            name: row.get("name"),
            description: row.get("description"),
            org: row.get("org_slug"),
            owner_email: row.get("owner_email"),
            visibility,
            team: row.get("team_slug"),
            version: row.get("version"),
            hash: PackageHash {
                algorithm: row.get("hash_algorithm"),
                hex: row.get("hash_hex"),
            },
            platform_tags: platform_tags_from_row(&row)?,
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
            status,
            current,
            yanked_at: row.get("yanked_at"),
            yank_reason: row.get("yank_reason"),
            deprecated_at: row.get("deprecated_at"),
            deprecation_reason: row.get("deprecation_reason"),
            audit_event_id: None,
        },
        storage_key: row.get("storage_key"),
    }))
}

pub(crate) async fn visible_versions(
    db: &DbPool,
    user: &AuthenticatedUser,
    org: &str,
    skill: &str,
) -> Result<Vec<VersionInfo>, ServerError> {
    let Some(skill_row) = sqlx::query(
        "SELECT skills.id AS skill_id, skills.visibility, skills.current_version_id,
                teams.slug AS team_slug,
                skills.owner_user_id,
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
    .bind(org)
    .bind(skill)
    .fetch_optional(db)
    .await
    .map_err(map_sql)?
    else {
        return Err(ServerError::skill_not_found(format!(
            "no such skill `{org}/{skill}`"
        )));
    };

    let visibility = visibility_from_db(skill_row.get::<String, _>("visibility").as_str())?;
    let role = role_for(user, org).ok_or_else(|| ServerError::forbidden("permission denied"))?;
    let owner_user_id: Option<String> = skill_row.get("owner_user_id");
    let team_role: Option<String> = skill_row.get("team_role");
    if !can_read_visibility(
        user,
        role,
        visibility,
        owner_user_id.as_deref(),
        team_role.is_some(),
    ) {
        return Err(ServerError::skill_not_found(format!(
            "no such skill `{org}/{skill}`"
        )));
    }

    let skill_id: String = skill_row.get("skill_id");
    let current_version_id: Option<String> = skill_row.get("current_version_id");
    let rows = sqlx::query(
        "SELECT skill_versions.id AS version_id, skill_versions.version_number::text AS version,
                skill_versions.status,
                to_char(skill_versions.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at,
                COALESCE((
                    SELECT jsonb_agg(tag ORDER BY tag)
                    FROM skill_version_platform_tags
                    WHERE skill_version_id = skill_versions.id
                ), '[]'::jsonb)::text AS platform_tags_json,
                to_char(skill_versions.yanked_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS yanked_at,
                skill_versions.yank_reason,
                to_char(skill_versions.deprecated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS deprecated_at,
                skill_versions.deprecation_reason,
                archives.hash_algorithm, archives.hash_hex
         FROM skill_versions
         JOIN archives ON archives.id = skill_versions.archive_id
         WHERE skill_versions.skill_id = $1
         ORDER BY skill_versions.version_number DESC",
    )
    .bind(&skill_id)
    .fetch_all(db)
    .await
    .map_err(map_sql)?;

    let mut versions = Vec::new();
    for row in rows {
        let status = version_status_from_db(row.get::<String, _>("status").as_str())?;
        if !can_read_version(
            user,
            role,
            visibility,
            owner_user_id.as_deref(),
            team_role.as_deref(),
            status,
        ) {
            continue;
        }
        let version_id: String = row.get("version_id");
        versions.push(VersionInfo {
            version: row.get("version"),
            hash: PackageHash {
                algorithm: row.get("hash_algorithm"),
                hex: row.get("hash_hex"),
            },
            platform_tags: platform_tags_from_row(&row)?,
            created_at: row.get("created_at"),
            status,
            current: current_version_id.as_deref() == Some(version_id.as_str()),
            yanked_at: row.get("yanked_at"),
            yank_reason: row.get("yank_reason"),
            deprecated_at: row.get("deprecated_at"),
            deprecation_reason: row.get("deprecation_reason"),
        });
    }
    Ok(versions)
}

fn platform_tags_from_row(row: &crate::db::DbRow) -> Result<Vec<String>, ServerError> {
    let raw: String = row.get("platform_tags_json");
    platform_tags_from_json(&raw)
}

pub(crate) fn parse_version_number(version: &str) -> Result<i64, ServerError> {
    let parsed = version.parse::<i64>().map_err(|_| {
        ServerError::validation_error(format!(
            "version `{version}` is not a valid integer version number"
        ))
    })?;
    if parsed <= 0 {
        return Err(ServerError::validation_error(format!(
            "version `{version}` is not a valid positive integer version number"
        )));
    }
    Ok(parsed)
}

fn platform_tags_from_json(raw: &str) -> Result<Vec<String>, ServerError> {
    serde_json::from_str(raw).map_err(|_| ServerError::internal_error())
}

fn no_current_version_error(org: &str, skill: &str) -> ServerError {
    ServerError::no_current_version(format!(
        "`{org}/{skill}` has uploaded candidate versions but no approved/current version yet; ask an org admin to run `agentstack skill version approve {org}/{skill}@<VERSION>`"
    ))
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

pub(crate) fn visibility_from_db(value: &str) -> Result<Visibility, ServerError> {
    match value {
        "private" => Ok(Visibility::Private),
        "org" => Ok(Visibility::Org),
        "team" => Ok(Visibility::Team),
        _ => {
            tracing::error!(visibility = value, "unknown visibility in database");
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

#[cfg(test)]
mod tests {
    use super::parse_version_number;

    #[test]
    fn parse_version_number_rejects_non_integer_values() {
        assert_eq!(parse_version_number("42").unwrap(), 42);

        let err = parse_version_number("1.2.3").unwrap_err();
        assert_eq!(err.code(), "validation_error");

        let err = parse_version_number("-1").unwrap_err();
        assert_eq!(err.code(), "validation_error");

        let err = parse_version_number("0").unwrap_err();
        assert_eq!(err.code(), "validation_error");
    }
}
