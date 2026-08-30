use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Visibility {
    Private,
    Org,
    Team,
}

impl Visibility {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Org => "org",
            Self::Team => "team",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VersionStatus {
    Candidate,
    Approved,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CatalogSort {
    Name,
    Updated,
    Owner,
}

impl CatalogSort {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "name" => Ok(Self::Name),
            "updated" => Ok(Self::Updated),
            "owner" => Ok(Self::Owner),
            other => Err(format!(
                "unknown sort `{other}` (expected one of: name, updated, owner)"
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PackageHash {
    pub(crate) algorithm: String,
    pub(crate) hex: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SkillMetadata {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) org: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) owner_email: Option<String>,
    pub(crate) visibility: Visibility,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) team: Option<String>,
    pub(crate) version: String,
    pub(crate) hash: PackageHash,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) platform_tags: Vec<String>,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    pub(crate) status: VersionStatus,
    pub(crate) current: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) yanked_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) yank_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) deprecated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) deprecation_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) audit_event_id: Option<String>,
}

impl SkillMetadata {
    pub(crate) fn skill_ref(&self) -> String {
        format!("{}/{}@{}", self.org, self.name, self.version)
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RemoteSkill {
    pub(crate) org: String,
    pub(crate) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) owner_email: Option<String>,
    pub(crate) latest_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) current_version: Option<String>,
    pub(crate) description: String,
    pub(crate) visibility: Visibility,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) team: Option<String>,
    pub(crate) updated_at: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) platform_tags: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct VersionInfo {
    pub(crate) version: String,
    pub(crate) hash: PackageHash,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) platform_tags: Vec<String>,
    pub(crate) created_at: String,
    pub(crate) status: VersionStatus,
    pub(crate) current: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) yanked_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) yank_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) deprecated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) deprecation_reason: Option<String>,
}

pub(crate) struct StoredMetadata {
    pub(crate) metadata: SkillMetadata,
    pub(crate) storage_key: String,
}
