use std::sync::Arc;

use anyhow::{Context, bail};
use sha2::{Digest, Sha256};
use sqlx::Row;

use crate::{
    admin,
    blob_store::BlobStore,
    config::QuotaConfig,
    db::{DbPool, DbTransaction},
    registry::{archive::validate_archive_metadata_blocking, validate_slug},
};

const MAX_ARCHIVE_BYTES: usize = 50 * 1024 * 1024;
const SEED_SKILL: &str = "agentstack";
const SEED_VERSION: i64 = 1;
const SEED_DESCRIPTION: &str = "Use when a user wants to codify, version, install, update, share, or govern AgentStack skills and stacks — turning prompts into validated skills, building and rolling out stacks to a team, or inspecting what context is installed.";
const SEED_SOURCE: &str = "platform_provisioning";

pub const AGENTSTACK_SEED_ARCHIVE_SHA256: &str =
    include_str!(concat!(env!("OUT_DIR"), "/agentstack_seed.sha256"));
pub const AGENTSTACK_SEED_ARCHIVE_BYTES: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/agentstack_seed.tar.gz"));

#[derive(Debug)]
pub struct ProvisionedOrg {
    pub org_id: String,
    pub org_slug: String,
    pub org_name: String,
    pub owner_user_id: String,
    pub owner_email: String,
    pub owner_created: bool,
    pub seed_skill_id: String,
    pub seed_version_id: String,
    pub seed_archive_hash: String,
    pub seed_created: bool,
}

pub async fn provision_org_with_owner(
    db: &DbPool,
    blob_store: &dyn BlobStore,
    quotas: &QuotaConfig,
    org_slug: &str,
    org_name: Option<&str>,
    owner_email: &str,
    owner_name: Option<&str>,
) -> anyhow::Result<ProvisionedOrg> {
    validate_slug(org_slug).map_err(anyhow::Error::msg)?;
    let org_name = admin::normalize_optional(org_name).unwrap_or_else(|| org_slug.to_string());
    let owner_email = admin::normalize_email(owner_email)?;
    let owner_name = admin::normalize_optional(owner_name);
    validate_canonical_seed_archive()
        .await
        .context("canonical agentstack seed archive is invalid")?;

    if org_id_by_slug(db, org_slug).await?.is_some() {
        bail!("org `{org_slug}` already exists");
    }

    ensure_seed_blob(blob_store).await?;

    let mut tx = db.begin().await?;
    let owner_user_id = insert_owner(&mut tx, &owner_email, owner_name.as_deref()).await?;
    let org_id = insert_org(&mut tx, org_slug, &org_name).await?;
    enforce_member_quota(&mut tx, quotas, &org_id, org_slug).await?;
    grant_owner(&mut tx, &org_id, &owner_user_id).await?;
    let seeded =
        seed_agentstack_skill_in_tx(&mut tx, quotas, &org_id, org_slug, &owner_user_id).await?;
    tx.commit().await?;

    Ok(ProvisionedOrg {
        org_id,
        org_slug: org_slug.to_string(),
        org_name,
        owner_user_id,
        owner_email,
        owner_created: true,
        seed_skill_id: seeded.skill_id,
        seed_version_id: seeded.version_id,
        seed_archive_hash: AGENTSTACK_SEED_ARCHIVE_SHA256.trim().to_string(),
        seed_created: seeded.created,
    })
}

pub async fn validate_canonical_seed_archive() -> anyhow::Result<()> {
    if AGENTSTACK_SEED_ARCHIVE_BYTES.len() > MAX_ARCHIVE_BYTES {
        bail!(
            "canonical seed archive is {} bytes; limit is {MAX_ARCHIVE_BYTES}",
            AGENTSTACK_SEED_ARCHIVE_BYTES.len()
        );
    }
    let actual_hash = sha256_hex(AGENTSTACK_SEED_ARCHIVE_BYTES);
    let expected_hash = AGENTSTACK_SEED_ARCHIVE_SHA256.trim();
    if actual_hash != expected_hash {
        bail!("canonical seed archive hash mismatch");
    }
    validate_archive_metadata_blocking(
        Arc::from(AGENTSTACK_SEED_ARCHIVE_BYTES),
        SEED_SKILL.to_string(),
        SEED_DESCRIPTION.to_string(),
    )
    .await
    .map_err(|err| anyhow::anyhow!("{err}"))?;
    Ok(())
}

async fn ensure_seed_blob(blob_store: &dyn BlobStore) -> anyhow::Result<()> {
    let key = seed_storage_key();
    if blob_store
        .exists(&key)
        .await
        .with_context(|| format!("failed to check seed archive blob `{key}`"))?
    {
        return Ok(());
    }
    blob_store
        .put(&key, AGENTSTACK_SEED_ARCHIVE_BYTES)
        .await
        .with_context(|| format!("failed to store seed archive blob `{key}`"))?;
    Ok(())
}

async fn insert_owner(
    tx: &mut DbTransaction<'_>,
    email: &str,
    name: Option<&str>,
) -> anyhow::Result<String> {
    if sqlx::query("SELECT id FROM users WHERE email = $1")
        .bind(email)
        .fetch_optional(&mut **tx)
        .await?
        .is_some()
    {
        bail!(
            "owner user `{email}` already exists; use lower-level user/org grant commands for existing-user administration"
        );
    }

    let id = admin::random_id("usr");
    let principal_id = admin::random_id("prn");
    let display_name = name.unwrap_or(email);
    sqlx::query(
        "INSERT INTO users (id, email, name, created_at, updated_at)
         VALUES ($1, $2, $3, now(), now())",
    )
    .bind(&id)
    .bind(email)
    .bind(name)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("failed to create owner user `{email}`"))?;

    sqlx::query(
        "INSERT INTO principals
            (id, principal_type, display_name, is_server_admin, created_at, updated_at)
         VALUES ($1, 'human', $2, false, now(), now())",
    )
    .bind(&principal_id)
    .bind(display_name)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("failed to create owner principal for `{email}`"))?;

    sqlx::query(
        "INSERT INTO human_profiles
            (principal_id, user_id, email, name, created_at, updated_at)
         VALUES ($1, $2, $3, $4, now(), now())",
    )
    .bind(&principal_id)
    .bind(&id)
    .bind(email)
    .bind(name)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("failed to create owner human profile for `{email}`"))?;

    Ok(id)
}

async fn insert_org(tx: &mut DbTransaction<'_>, slug: &str, name: &str) -> anyhow::Result<String> {
    let id = admin::random_id("org");
    sqlx::query(
        "INSERT INTO orgs (id, slug, name, created_at, updated_at)
         VALUES ($1, $2, $3, now(), now())",
    )
    .bind(&id)
    .bind(slug)
    .bind(name)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("failed to create org `{slug}`"))?;
    Ok(id)
}

async fn grant_owner(
    tx: &mut DbTransaction<'_>,
    org_id: &str,
    owner_user_id: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO org_members (org_id, user_id, role, created_at, updated_at)
         VALUES ($1, $2, 'org_admin', now(), now())",
    )
    .bind(org_id)
    .bind(owner_user_id)
    .execute(&mut **tx)
    .await
    .context("failed to grant provisioned owner org_admin")?;
    Ok(())
}

async fn enforce_member_quota(
    tx: &mut DbTransaction<'_>,
    quotas: &QuotaConfig,
    org_id: &str,
    org_slug: &str,
) -> anyhow::Result<()> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM org_members WHERE org_id = $1")
        .bind(org_id)
        .fetch_one(&mut **tx)
        .await?;
    if count >= quotas.max_org_members_per_org {
        bail!(
            "org `{org_slug}` has reached the member limit of {}",
            quotas.max_org_members_per_org
        );
    }
    Ok(())
}

struct SeededSkill {
    skill_id: String,
    version_id: String,
    created: bool,
}

async fn seed_agentstack_skill_in_tx(
    tx: &mut DbTransaction<'_>,
    quotas: &QuotaConfig,
    org_id: &str,
    org_slug: &str,
    owner_user_id: &str,
) -> anyhow::Result<SeededSkill> {
    if let Some(existing) = existing_seed(tx, org_id).await? {
        return Ok(existing);
    }

    enforce_seed_skill_quotas(tx, quotas, org_id, org_slug, owner_user_id).await?;

    let archive_id = ensure_archive_row(tx).await?;
    let skill_id = admin::random_id("skl");
    let version_id = admin::random_id("ver");

    sqlx::query(
        "INSERT INTO skills
            (id, org_id, name, description, visibility, team_id, owner_user_id)
         VALUES
            ($1, $2, $3, $4, 'org', NULL, $5)",
    )
    .bind(&skill_id)
    .bind(org_id)
    .bind(SEED_SKILL)
    .bind(SEED_DESCRIPTION)
    .bind(owner_user_id)
    .execute(&mut **tx)
    .await
    .context("failed to insert canonical agentstack skill")?;

    sqlx::query(
        "INSERT INTO skill_versions
            (id, skill_id, version_number, archive_id, description,
             published_by_user_id, status, approved_by_user_id, approved_at)
         VALUES
            ($1, $2, $3, $4, $5, $6, 'approved', $6, now())",
    )
    .bind(&version_id)
    .bind(&skill_id)
    .bind(SEED_VERSION)
    .bind(&archive_id)
    .bind(SEED_DESCRIPTION)
    .bind(owner_user_id)
    .execute(&mut **tx)
    .await
    .context("failed to insert canonical agentstack version")?;

    sqlx::query(
        "UPDATE skills
         SET current_version_id = $1,
             updated_at = now()
         WHERE id = $2",
    )
    .bind(&version_id)
    .bind(&skill_id)
    .execute(&mut **tx)
    .await
    .context("failed to mark canonical agentstack version current")?;

    insert_seed_audits(tx, org_id, owner_user_id, &skill_id, &version_id).await?;

    Ok(SeededSkill {
        skill_id,
        version_id,
        created: true,
    })
}

async fn enforce_seed_skill_quotas(
    tx: &mut DbTransaction<'_>,
    quotas: &QuotaConfig,
    org_id: &str,
    org_slug: &str,
    owner_user_id: &str,
) -> anyhow::Result<()> {
    let org_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM skills WHERE org_id = $1")
        .bind(org_id)
        .fetch_one(&mut **tx)
        .await?;
    if org_count >= quotas.max_skills_per_org {
        bail!(
            "org `{org_slug}` has reached the skill limit of {}",
            quotas.max_skills_per_org
        );
    }

    let owner_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM skills WHERE org_id = $1 AND owner_user_id = $2")
            .bind(org_id)
            .bind(owner_user_id)
            .fetch_one(&mut **tx)
            .await?;
    if owner_count >= quotas.max_skills_per_owner_per_org {
        bail!(
            "owner has reached the per-owner skill limit of {} in org `{org_slug}`",
            quotas.max_skills_per_owner_per_org
        );
    }

    // The seed skill is created with zero existing versions, so only a
    // non-positive quota can block its first version.
    if quotas.max_versions_per_skill <= 0 {
        bail!(
            "skill has reached the version limit of {}",
            quotas.max_versions_per_skill
        );
    }
    Ok(())
}

async fn existing_seed(
    tx: &mut DbTransaction<'_>,
    org_id: &str,
) -> anyhow::Result<Option<SeededSkill>> {
    let Some(row) = sqlx::query(
        "SELECT skills.id AS skill_id, skills.current_version_id,
                skill_versions.id AS version_id, skill_versions.status
         FROM skills
         JOIN skill_versions ON skill_versions.skill_id = skills.id
         WHERE skills.org_id = $1
           AND skills.name = $2
           AND skill_versions.version_number = $3",
    )
    .bind(org_id)
    .bind(SEED_SKILL)
    .bind(SEED_VERSION)
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Ok(None);
    };

    let skill_id: String = row.get("skill_id");
    let version_id: String = row.get("version_id");
    let current_version_id: Option<String> = row.get("current_version_id");
    let status: String = row.get("status");
    if status != "approved" || current_version_id.as_deref() != Some(version_id.as_str()) {
        bail!("canonical `{SEED_SKILL}@1` exists but is not the approved current version");
    }
    Ok(Some(SeededSkill {
        skill_id,
        version_id,
        created: false,
    }))
}

async fn ensure_archive_row(tx: &mut DbTransaction<'_>) -> anyhow::Result<String> {
    let hash = AGENTSTACK_SEED_ARCHIVE_SHA256.trim();
    if let Some(row) = sqlx::query(
        "SELECT id FROM archives
         WHERE hash_algorithm = 'sha256' AND hash_hex = $1",
    )
    .bind(hash)
    .fetch_optional(&mut **tx)
    .await?
    {
        return Ok(row.get("id"));
    }

    let id = admin::random_id("arc");
    sqlx::query(
        "INSERT INTO archives
            (id, hash_algorithm, hash_hex, storage_key, size_bytes)
         VALUES
            ($1, 'sha256', $2, $3, $4)",
    )
    .bind(&id)
    .bind(hash)
    .bind(seed_storage_key())
    .bind(AGENTSTACK_SEED_ARCHIVE_BYTES.len() as i64)
    .execute(&mut **tx)
    .await
    .context("failed to insert canonical agentstack archive row")?;
    Ok(id)
}

async fn insert_seed_audits(
    tx: &mut DbTransaction<'_>,
    org_id: &str,
    owner_user_id: &str,
    skill_id: &str,
    version_id: &str,
) -> anyhow::Result<()> {
    let metadata = serde_json::json!({
        "source": SEED_SOURCE,
        "version": "1"
    });
    insert_audit(
        tx,
        org_id,
        owner_user_id,
        skill_id,
        "skill.version_pushed",
        metadata.clone(),
    )
    .await?;
    insert_audit(
        tx,
        org_id,
        owner_user_id,
        skill_id,
        "skill.version_approved",
        metadata,
    )
    .await?;
    insert_audit(
        tx,
        org_id,
        owner_user_id,
        skill_id,
        "skill.current_changed",
        serde_json::json!({
            "source": SEED_SOURCE,
            "version": "1",
            "previous_current_version_id": null,
            "current_version_id": version_id,
        }),
    )
    .await?;
    Ok(())
}

async fn insert_audit(
    tx: &mut DbTransaction<'_>,
    org_id: &str,
    owner_user_id: &str,
    skill_id: &str,
    action: &'static str,
    metadata: serde_json::Value,
) -> anyhow::Result<String> {
    let id = admin::random_id("aud");
    sqlx::query(
        "INSERT INTO audit_log
            (id, org_id, actor_user_id, actor_principal_id, actor_type, action,
             resource_type, resource_id, metadata)
         VALUES
            ($1, $2, $3,
             (SELECT principal_id FROM human_profiles WHERE user_id = $3),
             'human', $4, 'skill', $5, $6::jsonb)",
    )
    .bind(&id)
    .bind(org_id)
    .bind(owner_user_id)
    .bind(action)
    .bind(skill_id)
    .bind(metadata.to_string())
    .execute(&mut **tx)
    .await
    .context("failed to insert canonical agentstack audit row")?;
    Ok(id)
}

async fn org_id_by_slug(db: &DbPool, slug: &str) -> anyhow::Result<Option<String>> {
    let row = sqlx::query("SELECT id FROM orgs WHERE slug = $1")
        .bind(slug)
        .fetch_optional(db)
        .await?;
    Ok(row.map(|row| row.get("id")))
}

fn seed_storage_key() -> String {
    let hash = AGENTSTACK_SEED_ARCHIVE_SHA256.trim();
    format!("sha256/{}/{}.tar.gz", &hash[..2], hash)
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex_encode(&Sha256::digest(bytes))
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
