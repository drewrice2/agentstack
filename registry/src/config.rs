use std::{net::SocketAddr, path::PathBuf};

use thiserror::Error;

const DEFAULT_BIND: &str = "127.0.0.1:8080";
const DEFAULT_DATABASE_URL: &str = "postgres://postgres:postgres@127.0.0.1:5432/agentstack";
const DEFAULT_BLOB_DIR: &str = "./agentstack-blobs";

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub database_url: String,
    pub blob_dir: PathBuf,
    pub quotas: QuotaConfig,
}

#[derive(Debug, Clone)]
pub struct QuotaConfig {
    pub max_teams_per_org: i64,
    pub max_team_members_per_team: i64,
    pub max_org_members_per_org: i64,
    pub max_skills_per_org: i64,
    pub max_skills_per_owner_per_org: i64,
    pub max_team_skills_per_team: i64,
    pub max_versions_per_skill: i64,
    pub max_stacks_per_org: i64,
    pub max_stack_items_per_stack: i64,
    pub max_active_tokens_per_user: i64,
    pub max_platform_tags_per_version: i64,
}

impl Default for QuotaConfig {
    fn default() -> Self {
        Self {
            max_teams_per_org: 25,
            max_team_members_per_team: 50,
            max_org_members_per_org: 100,
            max_skills_per_org: 500,
            max_skills_per_owner_per_org: 100,
            max_team_skills_per_team: 100,
            max_versions_per_skill: 100,
            max_stacks_per_org: 100,
            max_stack_items_per_stack: 50,
            max_active_tokens_per_user: 25,
            max_platform_tags_per_version: 10,
        }
    }
}

impl QuotaConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        let defaults = Self::default();
        Ok(Self {
            max_teams_per_org: env_positive_i64(
                "AGENTSTACK_MAX_TEAMS_PER_ORG",
                defaults.max_teams_per_org,
            )?,
            max_team_members_per_team: env_positive_i64(
                "AGENTSTACK_MAX_TEAM_MEMBERS_PER_TEAM",
                defaults.max_team_members_per_team,
            )?,
            max_org_members_per_org: env_positive_i64(
                "AGENTSTACK_MAX_ORG_MEMBERS_PER_ORG",
                defaults.max_org_members_per_org,
            )?,
            max_skills_per_org: env_positive_i64(
                "AGENTSTACK_MAX_SKILLS_PER_ORG",
                defaults.max_skills_per_org,
            )?,
            max_skills_per_owner_per_org: env_positive_i64(
                "AGENTSTACK_MAX_SKILLS_PER_OWNER_PER_ORG",
                defaults.max_skills_per_owner_per_org,
            )?,
            max_team_skills_per_team: env_positive_i64(
                "AGENTSTACK_MAX_TEAM_SKILLS_PER_TEAM",
                defaults.max_team_skills_per_team,
            )?,
            max_versions_per_skill: env_positive_i64(
                "AGENTSTACK_MAX_VERSIONS_PER_SKILL",
                defaults.max_versions_per_skill,
            )?,
            max_stacks_per_org: env_positive_i64(
                "AGENTSTACK_MAX_STACKS_PER_ORG",
                defaults.max_stacks_per_org,
            )?,
            max_stack_items_per_stack: env_positive_i64(
                "AGENTSTACK_MAX_STACK_ITEMS_PER_STACK",
                defaults.max_stack_items_per_stack,
            )?,
            max_active_tokens_per_user: env_positive_i64(
                "AGENTSTACK_MAX_ACTIVE_TOKENS_PER_USER",
                defaults.max_active_tokens_per_user,
            )?,
            max_platform_tags_per_version: env_positive_i64(
                "AGENTSTACK_MAX_PLATFORM_TAGS_PER_VERSION",
                defaults.max_platform_tags_per_version,
            )?,
        })
    }
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let bind_raw =
            std::env::var("AGENTSTACK_SERVER_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());
        let bind = bind_raw
            .parse::<SocketAddr>()
            .map_err(|source| ConfigError::InvalidBind {
                value: bind_raw,
                source,
            })?;
        let database_url = std::env::var("AGENTSTACK_DATABASE_URL")
            .unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string());
        let blob_dir = env_optional_nonempty("AGENTSTACK_BLOB_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                env_optional_nonempty("AGENTSTACK_DATA_DIR")
                    .map(|path| PathBuf::from(path).join("blobs"))
            })
            .unwrap_or_else(|| PathBuf::from(DEFAULT_BLOB_DIR));

        Ok(Self {
            bind,
            database_url,
            blob_dir,
            quotas: QuotaConfig::from_env()?,
        })
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid AGENTSTACK_SERVER_BIND '{value}': {source}")]
    InvalidBind {
        value: String,
        #[source]
        source: std::net::AddrParseError,
    },
    #[error("invalid {name} '{value}': expected a positive integer")]
    InvalidQuota { name: &'static str, value: String },
}

fn env_optional_nonempty(name: &'static str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn env_positive_i64(name: &'static str, default: i64) -> Result<i64, ConfigError> {
    match std::env::var(name) {
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(std::env::VarError::NotUnicode(value)) => Err(ConfigError::InvalidQuota {
            name,
            value: value.to_string_lossy().into_owned(),
        }),
        Ok(value) => {
            let trimmed = value.trim();
            trimmed
                .parse::<i64>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or(ConfigError::InvalidQuota {
                    name,
                    value: value.clone(),
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, ConfigError, QuotaConfig, env_positive_i64};
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clear_env() {
        for name in [
            "AGENTSTACK_SERVER_BIND",
            "AGENTSTACK_DATABASE_URL",
            "AGENTSTACK_BLOB_DIR",
            "AGENTSTACK_DATA_DIR",
            "AGENTSTACK_MAX_TEAMS_PER_ORG",
            "AGENTSTACK_MAX_TEAM_MEMBERS_PER_TEAM",
            "AGENTSTACK_MAX_ORG_MEMBERS_PER_ORG",
            "AGENTSTACK_MAX_SKILLS_PER_ORG",
            "AGENTSTACK_MAX_SKILLS_PER_OWNER_PER_ORG",
            "AGENTSTACK_MAX_TEAM_SKILLS_PER_TEAM",
            "AGENTSTACK_MAX_VERSIONS_PER_SKILL",
            "AGENTSTACK_MAX_STACKS_PER_ORG",
            "AGENTSTACK_MAX_STACK_ITEMS_PER_STACK",
            "AGENTSTACK_MAX_ACTIVE_TOKENS_PER_USER",
            "AGENTSTACK_MAX_PLATFORM_TAGS_PER_VERSION",
        ] {
            unsafe { std::env::remove_var(name) };
        }
    }

    #[test]
    fn config_defaults_to_local_postgres_and_filesystem_blobs() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();

        let config = Config::from_env().unwrap();

        assert_eq!(config.bind.to_string(), "127.0.0.1:8080");
        assert_eq!(
            config.database_url,
            "postgres://postgres:postgres@127.0.0.1:5432/agentstack"
        );
        assert_eq!(
            config.blob_dir,
            std::path::PathBuf::from("./agentstack-blobs")
        );
        assert_eq!(config.quotas.max_active_tokens_per_user, 25);
    }

    #[test]
    fn config_rejects_invalid_quota_values() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        unsafe { std::env::set_var("AGENTSTACK_MAX_STACK_ITEMS_PER_STACK", "0") };

        let err = Config::from_env().unwrap_err();
        assert!(matches!(
            err,
            ConfigError::InvalidQuota {
                name: "AGENTSTACK_MAX_STACK_ITEMS_PER_STACK",
                value
            } if value == "0"
        ));

        clear_env();
    }

    #[test]
    fn env_positive_i64_uses_default_when_missing() {
        let _guard = ENV_LOCK.lock().unwrap();
        let name = "AGENTSTACK_TEST_QUOTA_MISSING_XYZZY";
        unsafe { std::env::remove_var(name) };
        assert_eq!(env_positive_i64(name, 7).unwrap(), 7);
    }

    #[test]
    fn quota_defaults_match_documented_values() {
        let q = QuotaConfig::default();
        assert_eq!(q.max_teams_per_org, 25);
        assert_eq!(q.max_team_members_per_team, 50);
        assert_eq!(q.max_org_members_per_org, 100);
        assert_eq!(q.max_skills_per_org, 500);
        assert_eq!(q.max_skills_per_owner_per_org, 100);
        assert_eq!(q.max_team_skills_per_team, 100);
        assert_eq!(q.max_versions_per_skill, 100);
        assert_eq!(q.max_stacks_per_org, 100);
        assert_eq!(q.max_stack_items_per_stack, 50);
        assert_eq!(q.max_active_tokens_per_user, 25);
        assert_eq!(q.max_platform_tags_per_version, 10);
    }
}
