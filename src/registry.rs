//! Hosted-registry client surface.
//!
//! The CLI talks to a registry through the [`RegistryClient`] trait so
//! command code can be written and tested before the real backend lands.
//! Two implementations ship today:
//!
//! - [`HttpRegistryClient`] — production default backed by the documented
//!   `/v1` registry API.
//! - [`MockRegistryClient`] — backed by an in-memory store. Used by tests
//!   to verify command flow without standing up a real server.
//!
//! [`validate_registry_url`] enforces the URL shape the CLI accepts from
//! persisted config, `AGENTSTACK_REGISTRY_URL`, or the built-in default.
//!
//! The wire shapes — [`PingResponse`], [`WhoamiResponse`], [`SkillMetadata`],
//! [`PushRequest`], [`PushResponse`], [`PullResponse`], [`SearchResult`],
//! [`RemoteSkill`], [`VersionInfo`] — are the canonical contract documented
//! in `docs/API_CONTRACT.md`.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::Method;
use reqwest::blocking::{Client as BlockingHttpClient, RequestBuilder, Response};
use reqwest::header::HeaderMap;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

use crate::credentials::Token;
use crate::error::CliError;
use crate::package::PackageHash;
use crate::skill_ref::SkillRef;

const REGISTRY_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REGISTRY_IO_TIMEOUT: Duration = Duration::from_secs(120);
const REGISTRY_USER_AGENT: &str = concat!("agentstack/", env!("CARGO_PKG_VERSION"));

fn require_registry_version_number(version: &str) -> Result<&str> {
    if version.is_empty()
        || !version.chars().all(|c| c.is_ascii_digit())
        || !matches!(version.parse::<i64>(), Ok(value) if value > 0)
    {
        bail!("registry version `{version}` must be a positive integer");
    }
    Ok(version)
}

/// Per-call configuration: where to talk and (optionally) which token to use.
#[derive(Debug, Clone)]
pub struct RegistryConnection {
    pub url: String,
    pub token: Option<Token>,
}

impl RegistryConnection {
    pub fn new(url: impl Into<String>, token: Option<Token>) -> Self {
        Self {
            url: url.into(),
            token,
        }
    }
}

/// Reply from `ping` — just enough to confirm the server is reachable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PingResponse {
    /// Health status. V1 servers currently return `ok`.
    pub status: String,
    /// Server-reported version string.
    pub server_version: String,
}

/// Reply from `whoami` — identity attached to the current token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhoamiResponse {
    pub user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
    pub email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub server_admin: bool,
    pub orgs: Vec<OrgMembership>,
}

/// One organization membership attached to a `whoami` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgMembership {
    pub slug: String,
    pub name: String,
    pub role: String,
}

/// Browser-login start request. The CLI owns PKCE and loopback state; the
/// registry owns provider selection and the hosted identity mapping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthStartRequest {
    pub provider: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub state: String,
    pub client: String,
    pub cli_version: String,
}

/// Browser-login start response. `authorization_url` points at the provider
/// consent page or an AgentStack-hosted authorization bridge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthStartResponse {
    pub authorization_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

/// Browser-login exchange request. The code verifier is secret-equivalent and
/// must never be logged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthExchangeRequest {
    pub grant_type: String,
    pub provider: String,
    pub code: String,
    pub state: String,
    pub redirect_uri: String,
    pub code_verifier: String,
}

/// Browser-login exchange response. The token is an AgentStack registry token,
/// not a Google or upstream-provider access token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthExchangeResponse {
    pub token_type: String,
    pub access_token: String,
}

/// Visibility tier for a published skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    /// Everyone in the owning org can read and pull.
    Org,
    /// Only the original publisher/owner and admins can read or pull.
    Private,
    /// Members of one team, plus admins, can read and pull.
    Team,
}

impl Visibility {
    pub const fn as_str(self) -> &'static str {
        match self {
            Visibility::Org => "org",
            Visibility::Private => "private",
            Visibility::Team => "team",
        }
    }
}

impl std::fmt::Display for Visibility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Visibility {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "private" => Ok(Visibility::Private),
            "org" => Ok(Visibility::Org),
            "team" => Ok(Visibility::Team),
            other => bail!("unknown visibility `{other}` (expected one of: private, org, team)"),
        }
    }
}

/// Version selection policy for a skill inside a stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionPolicy {
    Current,
    Pinned,
}

impl VersionPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            VersionPolicy::Current => "current",
            VersionPolicy::Pinned => "pinned",
        }
    }
}

impl std::fmt::Display for VersionPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for VersionPolicy {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "current" => Ok(VersionPolicy::Current),
            "pinned" => Ok(VersionPolicy::Pinned),
            other => bail!("unknown version policy `{other}` (expected one of: current, pinned)"),
        }
    }
}

/// Registry lifecycle state for one uploaded skill version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionStatus {
    Candidate,
    Approved,
    Rejected,
}

impl VersionStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            VersionStatus::Candidate => "candidate",
            VersionStatus::Approved => "approved",
            VersionStatus::Rejected => "rejected",
        }
    }
}

impl std::fmt::Display for VersionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Canonical metadata for a published skill version. The CLI sends this on
/// push and the server echoes it back on pull/search/list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub org: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    pub visibility: Visibility,
    pub version: String,
    pub hash: PackageHash,
    /// Optional platform tags (e.g. `claude-code`, `codex`). Empty by default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platform_tags: Vec<String>,
    /// Server-set creation timestamp (RFC3339). Absent until the server replies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// Server-set last-modified timestamp (RFC3339). Absent until the server replies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// Server-tracked count of archive downloads served for this skill across
    /// all versions. Optional; servers may omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_count: Option<u64>,
    /// Server-set timestamp (RFC3339) of the most recent archive download.
    /// Optional; absent when never downloaded or when the server omits it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_installed_at: Option<String>,
    /// Server-set lifecycle state. Omitted from push requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<VersionStatus>,
    /// Whether this version is the current approved version for the skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<bool>,
    /// Server-set timestamp this version was yanked, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yanked_at: Option<String>,
    /// Reason an admin recorded when yanking, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yank_reason: Option<String>,
    /// Server-set timestamp this version was deprecated, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated_at: Option<String>,
    /// Reason an admin recorded when deprecating, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecation_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_event_id: Option<String>,
}

impl SkillMetadata {
    /// `org/name@version` form.
    pub fn skill_ref(&self) -> String {
        format!("{}/{}@{}", self.org, self.name, self.version)
    }
}

/// Body of a push request — metadata plus the gzipped tar bytes.
#[derive(Debug, Clone)]
pub struct PushRequest<'a> {
    pub metadata: SkillMetadata,
    pub archive: &'a [u8],
}

/// Reply from `push` — where the published artifact lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushResponse {
    /// Canonical metadata as the server stored it (includes timestamps).
    pub metadata: SkillMetadata,
    /// Fully-qualified `org/name@version` reference for the stored artifact.
    pub skill_ref: String,
    /// Server-assigned version for the stored artifact.
    pub version: String,
    /// Hex-encoded SHA-256 of the stored archive bytes.
    pub sha256: String,
    /// Visibility tier for the stored artifact.
    pub visibility: Visibility,
    /// Optional URL pointing at the published skill (browser-friendly).
    #[serde(default)]
    pub url: Option<String>,
    /// Audit event recorded for the remote mutation, when returned by the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_event_id: Option<String>,
}

/// Reply from `pull` — the archive bytes plus the metadata the server stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullResponse {
    pub metadata: SkillMetadata,
    pub archive: Vec<u8>,
}

/// One row in `search` results — the same catalog row shape as
/// [`RemoteSkill`].
pub type SearchResult = RemoteSkill;

/// Sort order accepted by registry catalog list/search calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSort {
    Name,
    Updated,
    Owner,
    Installs,
}

impl CatalogSort {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Updated => "updated",
            Self::Owner => "owner",
            Self::Installs => "installs",
        }
    }
}

impl std::str::FromStr for CatalogSort {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "name" => Ok(Self::Name),
            "updated" => Ok(Self::Updated),
            "owner" => Ok(Self::Owner),
            "installs" => Ok(Self::Installs),
            other => {
                anyhow::bail!(
                    "unknown sort `{other}` (expected one of: name, updated, owner, installs)"
                )
            }
        }
    }
}

/// Optional filters accepted by registry search.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<Visibility>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<CatalogSort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

impl SearchFilters {
    pub fn is_empty(&self) -> bool {
        self.org.is_none()
            && self.team.is_none()
            && self.platforms.is_empty()
            && self.visibility.is_none()
            && self.owner.is_none()
            && self.sort.is_none()
            && self.limit.is_none()
    }
}

/// One catalog row — the server's view of a remote skill, returned by
/// `list_remote` and (as [`SearchResult`]) by `search`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteSkill {
    pub org: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_email: Option<String>,
    pub latest_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_version: Option<String>,
    pub description: String,
    pub visibility: Visibility,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platform_tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_installed_at: Option<String>,
}

impl RemoteSkill {
    pub fn skill_ref(&self) -> String {
        format!("{}/{}", self.org, self.name)
    }
}

/// One team row visible to the authenticated registry user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamSummary {
    pub org: String,
    pub slug: String,
}

/// One member in a team detail response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMember {
    pub email: String,
    pub role: String,
}

/// Full team detail, including members.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamDetail {
    pub org: String,
    pub slug: String,
    pub members: Vec<TeamMember>,
    #[serde(skip)]
    pub audit_event_id: Option<String>,
}

/// One stack row visible to the authenticated registry user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackSummary {
    pub org: String,
    pub slug: String,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_email: Option<String>,
    pub visibility: Visibility,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    pub item_count: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackListFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// One skill entry in a stack definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackItemSummary {
    pub skill: String,
    pub version_policy: VersionPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_version: Option<String>,
    pub position: i64,
    pub added_at: String,
}

/// Full stack definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackDetail {
    pub org: String,
    pub slug: String,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_email: Option<String>,
    pub visibility: Visibility,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub items: Vec<StackItemSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_event_id: Option<String>,
}

/// Stack identity included in a resolved manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackResolveHeader {
    pub org: String,
    pub slug: String,
    pub name: String,
    pub visibility: Visibility,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
}

/// Download route supplied by the registry for a resolved stack item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackDownloadRoute {
    pub method: String,
    pub url: String,
}

/// One concrete skill version in a resolved stack manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackResolvedItem {
    pub skill: String,
    pub version_id: String,
    pub version: String,
    pub archive_hash: PackageHash,
    pub download: StackDownloadRoute,
    pub version_policy: VersionPolicy,
}

/// Resolved stack manifest used by stack pull/install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackResolve {
    pub stack: StackResolveHeader,
    pub resolved_at: String,
    pub manifest_hash: PackageHash,
    pub items: Vec<StackResolvedItem>,
}

/// One entry in `list_versions` — a single uploaded version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    pub version: String,
    pub hash: PackageHash,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platform_tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<VersionStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yanked_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yank_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecation_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillStatus {
    pub skill: RemoteSkill,
    pub versions: Vec<VersionInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillImpact {
    pub skill: RemoteSkill,
    pub summary: SkillImpactSummary,
    pub used_by: Vec<SkillImpactStack>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillImpactSummary {
    pub used_by_count: usize,
    pub current_policy_count: usize,
    pub pinned_count: usize,
    pub visible_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillImpactStack {
    pub stack: String,
    pub org: String,
    pub slug: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_email: Option<String>,
    pub visibility: Visibility,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    pub version_policy: VersionPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<VersionStatus>,
    pub current: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yanked_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yank_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecation_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibilityStatus {
    pub org: String,
    pub skill: String,
    pub visibility: Visibility,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_event_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackStatus {
    pub stack: StackDetail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    pub id: String,
    pub org: String,
    pub action: String,
    pub resource_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_email: Option<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    pub created_at: String,
}

/// Operations a hosted-registry backend must provide. All methods are
/// fallible — implementations should surface network and auth errors with
/// enough context that the CLI can show a clean message.
pub trait RegistryClient {
    fn ping(&self) -> Result<PingResponse>;
    fn whoami(&self) -> Result<WhoamiResponse>;
    fn push(&self, request: PushRequest<'_>) -> Result<PushResponse>;
    fn pull(&self, skill_ref: &SkillRef) -> Result<PullResponse> {
        self.pull_with_options(skill_ref, PullClientOptions::default())
    }
    fn pull_with_options(
        &self,
        skill_ref: &SkillRef,
        options: PullClientOptions,
    ) -> Result<PullResponse>;
    fn approve(&self, skill_ref: &SkillRef, version: &str) -> Result<SkillMetadata>;
    fn yank(&self, skill_ref: &SkillRef, version: &str, reason: &str) -> Result<SkillMetadata>;
    fn deprecate(&self, skill_ref: &SkillRef, version: &str, reason: &str)
    -> Result<SkillMetadata>;
    fn search(&self, query: &str) -> Result<Vec<SearchResult>>;
    fn search_with_filters(
        &self,
        query: &str,
        filters: &SearchFilters,
    ) -> Result<Vec<SearchResult>> {
        if filters.is_empty() {
            self.search(query)
        } else {
            bail!("registry client does not support search filters")
        }
    }
    fn list_remote(&self, org: Option<&str>) -> Result<Vec<RemoteSkill>>;
    fn list_remote_with_filters(&self, filters: &SearchFilters) -> Result<Vec<RemoteSkill>> {
        if filters.team.is_none()
            && filters.platforms.is_empty()
            && filters.visibility.is_none()
            && filters.owner.is_none()
            && filters.sort.is_none()
        {
            self.list_remote(filters.org.as_deref())
        } else {
            bail!("registry client does not support list filters")
        }
    }
    fn list_versions(&self, skill_ref: &SkillRef) -> Result<Vec<VersionInfo>>;
    fn skill_metadata(&self, _skill_ref: &SkillRef) -> Result<SkillMetadata> {
        bail!("registry client does not support skill metadata")
    }
    fn skill_status(&self, _skill_ref: &SkillRef) -> Result<SkillStatus> {
        bail!("registry client does not support skill status")
    }
    fn skill_impact(&self, _skill_ref: &SkillRef) -> Result<SkillImpact> {
        bail!("registry client does not support skill impact")
    }
    fn skill_audit(&self, _skill_ref: &SkillRef) -> Result<Vec<AuditEvent>> {
        bail!("registry client does not support skill audit")
    }
    fn skill_visibility(&self, _skill_ref: &SkillRef) -> Result<VisibilityStatus> {
        bail!("registry client does not support skill visibility")
    }
    fn set_skill_visibility(
        &self,
        _skill_ref: &SkillRef,
        _visibility: Visibility,
        _team: Option<&str>,
    ) -> Result<VisibilityStatus> {
        bail!("registry client does not support skill visibility")
    }
    fn create_team(&self, _org: &str, _team: &str) -> Result<TeamDetail> {
        bail!("registry client does not support team management")
    }
    fn list_teams(&self, _org: &str) -> Result<Vec<TeamSummary>> {
        bail!("registry client does not support team management")
    }
    fn inspect_team(&self, _org: &str, _team: &str) -> Result<TeamDetail> {
        bail!("registry client does not support team management")
    }
    fn add_team_member(
        &self,
        _org: &str,
        _team: &str,
        _email: &str,
        _role: &str,
    ) -> Result<TeamDetail> {
        bail!("registry client does not support team management")
    }
    fn remove_team_member(&self, _org: &str, _team: &str, _email: &str) -> Result<TeamDetail> {
        bail!("registry client does not support team management")
    }
    fn set_team_role(
        &self,
        _org: &str,
        _team: &str,
        _email: &str,
        _role: &str,
    ) -> Result<TeamDetail> {
        bail!("registry client does not support team management")
    }
    fn create_stack(
        &self,
        _org: &str,
        _slug: &str,
        _name: &str,
        _description: &str,
        _visibility: Visibility,
        _team: Option<&str>,
    ) -> Result<StackDetail> {
        bail!("registry client does not support stack management")
    }
    fn list_stacks(&self, _org: &str) -> Result<Vec<StackSummary>> {
        bail!("registry client does not support stack management")
    }
    fn list_stacks_with_filters(
        &self,
        org: &str,
        filters: &StackListFilters,
    ) -> Result<Vec<StackSummary>> {
        let mut stacks = self.list_stacks(org)?;
        if let Some(owner) = filters.owner.as_deref() {
            stacks.retain(|stack| stack.owner_email.as_deref() == Some(owner));
        }
        if let Some(team) = filters.team.as_deref() {
            stacks.retain(|stack| stack.team.as_deref() == Some(team));
        }
        if let Some(limit) = filters.limit {
            stacks.truncate(limit);
        }
        Ok(stacks)
    }
    fn inspect_stack(&self, _org: &str, _stack: &str) -> Result<StackDetail> {
        bail!("registry client does not support stack management")
    }
    fn upsert_stack_item(
        &self,
        _org: &str,
        _stack: &str,
        _skill: &str,
        _version_policy: VersionPolicy,
        _pinned_version: Option<&str>,
    ) -> Result<StackDetail> {
        bail!("registry client does not support stack management")
    }
    fn remove_stack_item(&self, _org: &str, _stack: &str, _skill: &str) -> Result<StackDetail> {
        bail!("registry client does not support stack management")
    }
    fn resolve_stack(&self, _org: &str, _stack: &str) -> Result<StackResolve> {
        bail!("registry client does not support stack management")
    }
    fn set_stack_visibility(
        &self,
        _org: &str,
        _stack: &str,
        _visibility: Visibility,
        _team: Option<&str>,
    ) -> Result<StackDetail> {
        bail!("registry client does not support stack visibility")
    }
    fn stack_status(&self, _org: &str, _stack: &str) -> Result<StackStatus> {
        bail!("registry client does not support stack status")
    }
    fn stack_audit(&self, _org: &str, _stack: &str) -> Result<Vec<AuditEvent>> {
        bail!("registry client does not support stack audit")
    }
    fn org_audit(&self, _org: &str) -> Result<Vec<AuditEvent>> {
        bail!("registry client does not support org audit")
    }
    fn org_audit_event(&self, _org: &str, _event_id: &str) -> Result<AuditEvent> {
        bail!("registry client does not support org audit")
    }
}

/// Per-call options for pulling a skill archive.
#[derive(Debug, Default, Clone, Copy)]
pub struct PullClientOptions {
    /// Send `?allow_yanked=true` so the server permits download of yanked versions.
    pub allow_yanked: bool,
}

/// HTTP-backed registry client for the documented `/v1` API.
#[derive(Debug, Clone)]
pub struct HttpRegistryClient {
    connection: RegistryConnection,
    http: BlockingHttpClient,
}

impl HttpRegistryClient {
    pub fn new(connection: RegistryConnection) -> Self {
        Self {
            connection,
            http: Self::build_http_client(REGISTRY_CONNECT_TIMEOUT, REGISTRY_IO_TIMEOUT),
        }
    }

    fn build_http_client(connect_timeout: Duration, io_timeout: Duration) -> BlockingHttpClient {
        // reqwest::blocking exposes one non-connect timeout; it covers uploads
        // and response reads, so use one explicit I/O bound.
        //
        // Never follow redirects: the documented API has none, and following
        // one could leak the bearer token over a downgraded or relocated
        // connection.
        BlockingHttpClient::builder()
            .connect_timeout(connect_timeout)
            .timeout(io_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(REGISTRY_USER_AGENT)
            .build()
            .expect("static registry HTTP client configuration should be valid")
    }

    #[cfg(test)]
    fn with_timeouts(
        connection: RegistryConnection,
        connect_timeout: Duration,
        io_timeout: Duration,
    ) -> Self {
        Self {
            connection,
            http: Self::build_http_client(connect_timeout, io_timeout),
        }
    }

    fn api_url(&self, path: &str) -> Result<String> {
        debug_assert!(path.starts_with('/'));
        let registry_url = RegistryUrl::parse(&self.connection.url)?;
        Ok(registry_url.endpoint(path)?.to_string())
    }

    fn authenticated(&self, request: RequestBuilder) -> Result<RequestBuilder> {
        let token = self.connection.token.as_ref().ok_or_else(|| {
            CliError::new(
                "unauthenticated",
                "not logged in; run `agentstack auth login` or set AGENTSTACK_TOKEN",
            )
            .action("authenticate")
            .next_command("agentstack auth login")
            .machine_hint("set AGENTSTACK_TOKEN_PATH or AGENTSTACK_TOKEN for automation")
            .auth_methods(["auth_login", "AGENTSTACK_TOKEN_PATH", "AGENTSTACK_TOKEN"])
        })?;
        Ok(request.bearer_auth(token.expose_secret()))
    }

    fn get_json<T: DeserializeOwned>(&self, path: &str, authenticated: bool) -> Result<T> {
        let request = self.http.get(self.api_url(path)?);
        let request = if authenticated {
            self.authenticated(request)?
        } else {
            request
        };
        self.decode_json(request.send().context("registry request failed")?)
    }

    fn send_json<T: DeserializeOwned>(&self, method: Method, path: &str) -> Result<T> {
        let response = self
            .authenticated(self.http.request(method, self.api_url(path)?))?
            .send()
            .context("registry request failed")?;
        self.decode_json(response)
    }

    fn send_json_body<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        method: Method,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let response = self
            .authenticated(self.http.request(method, self.api_url(path)?))?
            .json(body)
            .send()
            .context("registry request failed")?;
        self.decode_json(response)
    }

    fn decode_json<T: DeserializeOwned>(&self, response: Response) -> Result<T> {
        if !response.status().is_success() {
            return Err(self.response_error(response));
        }
        response
            .json()
            .context("registry response was not valid JSON")
    }

    fn lifecycle_post(
        &self,
        skill_ref: &SkillRef,
        version: &str,
        action: &'static str,
        reason: &str,
    ) -> Result<SkillMetadata> {
        let version = require_registry_version_number(version)?;
        let path = format!(
            "/orgs/{}/skills/{}/versions/{}/{action}",
            encode_path_segment(&skill_ref.org),
            encode_path_segment(&skill_ref.name),
            encode_path_segment(version),
        );
        self.send_json_body(
            Method::POST,
            &path,
            &serde_json::json!({ "reason": reason }),
        )
    }

    fn decode_bytes(&self, response: Response) -> Result<(HeaderMap, Vec<u8>)> {
        self.decode_bytes_with_limit(response, crate::package::MAX_ARCHIVE_BYTES)
    }

    fn decode_bytes_with_limit(
        &self,
        response: Response,
        limit: usize,
    ) -> Result<(HeaderMap, Vec<u8>)> {
        if !response.status().is_success() {
            return Err(self.response_error(response));
        }
        if let Some(length) = response.content_length()
            && length > limit as u64
        {
            bail!("registry archive is {length} bytes; the download limit is {limit} bytes");
        }
        let headers = response.headers().clone();
        let mut body = response.take(limit.saturating_add(1) as u64);
        let mut bytes = Vec::new();
        body.read_to_end(&mut bytes)
            .context("failed to read registry response body")?;
        if bytes.len() > limit {
            bail!("registry archive exceeded the download limit of {limit} bytes");
        }
        Ok((headers, bytes))
    }

    fn response_error(&self, response: Response) -> anyhow::Error {
        response_error(response, self.connection.token.as_ref())
    }

    pub fn oauth_start(&self, request: &OAuthStartRequest) -> Result<OAuthStartResponse> {
        let response = self
            .http
            .post(self.api_url("/auth/oauth/start")?)
            .json(request)
            .send()
            .context("registry OAuth start request failed")?;
        self.decode_json(response)
    }

    pub fn oauth_exchange(&self, request: &OAuthExchangeRequest) -> Result<OAuthExchangeResponse> {
        let response = self
            .http
            .post(self.api_url("/auth/oauth/token")?)
            .json(request)
            .send()
            .context("registry OAuth exchange request failed")?;
        self.decode_json(response)
    }
}

impl RegistryClient for HttpRegistryClient {
    fn ping(&self) -> Result<PingResponse> {
        self.get_json("/ping", false)
    }

    fn whoami(&self) -> Result<WhoamiResponse> {
        self.get_json("/whoami", true)
    }

    fn push(&self, request: PushRequest<'_>) -> Result<PushResponse> {
        let metadata_json = serde_json::to_string(&request.metadata)
            .context("failed to serialize registry metadata")?;
        let metadata = reqwest::blocking::multipart::Part::text(metadata_json)
            .mime_str("application/json")
            .context("failed to build metadata multipart part")?;
        let archive = reqwest::blocking::multipart::Part::bytes(request.archive.to_vec())
            .file_name(format!("{}.tar.gz", request.metadata.name))
            .mime_str("application/gzip")
            .context("failed to build archive multipart part")?;
        let form = reqwest::blocking::multipart::Form::new()
            .part("metadata", metadata)
            .part("archive", archive);

        let path = format!(
            "/orgs/{}/skills",
            encode_path_segment(&request.metadata.org)
        );
        let response = self
            .authenticated(self.http.post(self.api_url(&path)?))?
            .multipart(form)
            .send()
            .context("registry request failed")?;
        self.decode_json(response)
    }

    fn pull_with_options(
        &self,
        skill_ref: &SkillRef,
        options: PullClientOptions,
    ) -> Result<PullResponse> {
        let metadata = self.skill_metadata(skill_ref)?;
        let archive_version = require_registry_version_number(&metadata.version)?;

        let mut archive_path = format!(
            "/orgs/{}/skills/{}/versions/{}/archive",
            encode_path_segment(&metadata.org),
            encode_path_segment(&metadata.name),
            encode_path_segment(archive_version)
        );
        if options.allow_yanked {
            archive_path.push_str("?allow_yanked=true");
        }
        let response = self
            .authenticated(self.http.get(self.api_url(&archive_path)?))?
            .send()
            .context("registry request failed")?;
        let (headers, archive) = self.decode_bytes(response)?;
        let actual = PackageHash::sha256_of(&archive);
        if actual != metadata.hash {
            bail!(
                "hash mismatch for {}: expected {} but archive bytes hash to {}",
                metadata.skill_ref(),
                metadata.hash.hex,
                actual.hex,
            );
        }
        if let Some(header) = headers.get("x-agentstack-sha256") {
            let declared = header
                .to_str()
                .context("registry sent malformed x-agentstack-sha256 header")?;
            if declared != metadata.hash.hex {
                bail!(
                    "hash header mismatch for {}: metadata says {} but header says {}",
                    metadata.skill_ref(),
                    metadata.hash.hex,
                    declared,
                );
            }
        }
        Ok(PullResponse { metadata, archive })
    }

    fn approve(&self, skill_ref: &SkillRef, version: &str) -> Result<SkillMetadata> {
        let version = require_registry_version_number(version)?;
        let path = format!(
            "/orgs/{}/skills/{}/versions/{}/approve",
            encode_path_segment(&skill_ref.org),
            encode_path_segment(&skill_ref.name),
            encode_path_segment(version)
        );
        self.send_json(Method::POST, &path)
    }

    fn yank(&self, skill_ref: &SkillRef, version: &str, reason: &str) -> Result<SkillMetadata> {
        self.lifecycle_post(skill_ref, version, "yank", reason)
    }

    fn deprecate(
        &self,
        skill_ref: &SkillRef,
        version: &str,
        reason: &str,
    ) -> Result<SkillMetadata> {
        self.lifecycle_post(skill_ref, version, "deprecate", reason)
    }

    fn search(&self, query: &str) -> Result<Vec<SearchResult>> {
        self.search_with_filters(query, &SearchFilters::default())
    }

    fn search_with_filters(
        &self,
        query: &str,
        filters: &SearchFilters,
    ) -> Result<Vec<SearchResult>> {
        let mut path = format!("/search?q={}", encode_query_component(query));
        let mut first = false;
        if let Some(org) = filters.org.as_deref() {
            append_query_param(&mut path, &mut first, "org", org);
        }
        if let Some(team) = filters.team.as_deref() {
            append_query_param(&mut path, &mut first, "team", team);
        }
        for platform in &filters.platforms {
            append_query_param(&mut path, &mut first, "platform", platform);
        }
        if let Some(visibility) = filters.visibility {
            append_query_param(&mut path, &mut first, "visibility", visibility.as_str());
        }
        if let Some(owner) = filters.owner.as_deref() {
            append_query_param(&mut path, &mut first, "owner", owner);
        }
        if let Some(sort) = filters.sort {
            append_query_param(&mut path, &mut first, "sort", sort.as_str());
        }
        if let Some(limit) = filters.limit {
            append_query_param(&mut path, &mut first, "limit", &limit.to_string());
        }
        let envelope: SearchEnvelope = self.get_json(&path, true)?;
        Ok(envelope.results)
    }

    fn list_remote(&self, org: Option<&str>) -> Result<Vec<RemoteSkill>> {
        let filters = SearchFilters {
            org: org.map(str::to_string),
            ..SearchFilters::default()
        };
        self.list_remote_with_filters(&filters)
    }

    fn list_remote_with_filters(&self, filters: &SearchFilters) -> Result<Vec<RemoteSkill>> {
        let mut path = match filters.org.as_deref() {
            Some(org) => format!("/orgs/{}/skills", encode_path_segment(org)),
            None => "/skills".to_string(),
        };
        let mut first = true;
        if let Some(team) = filters.team.as_deref() {
            append_query_param(&mut path, &mut first, "team", team);
        }
        for platform in &filters.platforms {
            append_query_param(&mut path, &mut first, "platform", platform);
        }
        if let Some(visibility) = filters.visibility {
            append_query_param(&mut path, &mut first, "visibility", visibility.as_str());
        }
        if let Some(owner) = filters.owner.as_deref() {
            append_query_param(&mut path, &mut first, "owner", owner);
        }
        if let Some(sort) = filters.sort {
            append_query_param(&mut path, &mut first, "sort", sort.as_str());
        }
        if let Some(limit) = filters.limit {
            append_query_param(&mut path, &mut first, "limit", &limit.to_string());
        }
        let envelope: SkillListEnvelope = self.get_json(&path, true)?;
        Ok(envelope.skills)
    }

    fn list_versions(&self, skill_ref: &SkillRef) -> Result<Vec<VersionInfo>> {
        let path = format!(
            "/orgs/{}/skills/{}/versions",
            encode_path_segment(&skill_ref.org),
            encode_path_segment(&skill_ref.name)
        );
        let envelope: VersionListEnvelope = self.get_json(&path, true)?;
        Ok(envelope.versions)
    }

    fn skill_metadata(&self, skill_ref: &SkillRef) -> Result<SkillMetadata> {
        let path = match skill_ref.version.as_deref() {
            Some(version) => {
                let version = require_registry_version_number(version)?;
                format!(
                    "/orgs/{}/skills/{}/versions/{}",
                    encode_path_segment(&skill_ref.org),
                    encode_path_segment(&skill_ref.name),
                    encode_path_segment(version)
                )
            }
            None => format!(
                "/orgs/{}/skills/{}",
                encode_path_segment(&skill_ref.org),
                encode_path_segment(&skill_ref.name)
            ),
        };
        self.get_json(&path, true)
    }

    fn skill_status(&self, skill_ref: &SkillRef) -> Result<SkillStatus> {
        let path = format!(
            "/orgs/{}/skills/{}/status",
            encode_path_segment(&skill_ref.org),
            encode_path_segment(&skill_ref.name)
        );
        self.get_json(&path, true)
    }

    fn skill_impact(&self, skill_ref: &SkillRef) -> Result<SkillImpact> {
        let path = format!(
            "/orgs/{}/skills/{}/impact",
            encode_path_segment(&skill_ref.org),
            encode_path_segment(&skill_ref.name)
        );
        self.get_json(&path, true)
    }

    fn skill_audit(&self, skill_ref: &SkillRef) -> Result<Vec<AuditEvent>> {
        let path = format!(
            "/orgs/{}/skills/{}/audit",
            encode_path_segment(&skill_ref.org),
            encode_path_segment(&skill_ref.name)
        );
        let envelope: AuditListEnvelope = self.get_json(&path, true)?;
        Ok(envelope.events)
    }

    fn skill_visibility(&self, skill_ref: &SkillRef) -> Result<VisibilityStatus> {
        let path = format!(
            "/orgs/{}/skills/{}/visibility",
            encode_path_segment(&skill_ref.org),
            encode_path_segment(&skill_ref.name)
        );
        self.get_json(&path, true)
    }

    fn set_skill_visibility(
        &self,
        skill_ref: &SkillRef,
        visibility: Visibility,
        team: Option<&str>,
    ) -> Result<VisibilityStatus> {
        let path = format!(
            "/orgs/{}/skills/{}/visibility",
            encode_path_segment(&skill_ref.org),
            encode_path_segment(&skill_ref.name)
        );
        self.send_json_body(
            Method::PATCH,
            &path,
            &serde_json::json!({ "visibility": visibility, "team": team }),
        )
    }

    fn create_team(&self, org: &str, team: &str) -> Result<TeamDetail> {
        let path = format!("/orgs/{}/teams", encode_path_segment(org));
        let envelope: TeamEnvelope =
            self.send_json_body(Method::POST, &path, &serde_json::json!({ "slug": team }))?;
        Ok(team_detail_with_audit(envelope))
    }

    fn list_teams(&self, org: &str) -> Result<Vec<TeamSummary>> {
        let path = format!("/orgs/{}/teams", encode_path_segment(org));
        let envelope: TeamListEnvelope = self.get_json(&path, true)?;
        Ok(envelope.teams)
    }

    fn inspect_team(&self, org: &str, team: &str) -> Result<TeamDetail> {
        let path = format!(
            "/orgs/{}/teams/{}",
            encode_path_segment(org),
            encode_path_segment(team)
        );
        let envelope: TeamEnvelope = self.get_json(&path, true)?;
        Ok(envelope.team)
    }

    fn add_team_member(
        &self,
        org: &str,
        team: &str,
        email: &str,
        role: &str,
    ) -> Result<TeamDetail> {
        let path = format!(
            "/orgs/{}/teams/{}/members/{}",
            encode_path_segment(org),
            encode_path_segment(team),
            encode_path_segment(email)
        );
        let envelope: TeamEnvelope =
            self.send_json_body(Method::PUT, &path, &serde_json::json!({ "role": role }))?;
        Ok(team_detail_with_audit(envelope))
    }

    fn remove_team_member(&self, org: &str, team: &str, email: &str) -> Result<TeamDetail> {
        let path = format!(
            "/orgs/{}/teams/{}/members/{}",
            encode_path_segment(org),
            encode_path_segment(team),
            encode_path_segment(email)
        );
        let envelope: TeamEnvelope = self.send_json(Method::DELETE, &path)?;
        Ok(team_detail_with_audit(envelope))
    }

    fn set_team_role(&self, org: &str, team: &str, email: &str, role: &str) -> Result<TeamDetail> {
        let path = format!(
            "/orgs/{}/teams/{}/members/{}",
            encode_path_segment(org),
            encode_path_segment(team),
            encode_path_segment(email)
        );
        let envelope: TeamEnvelope =
            self.send_json_body(Method::PATCH, &path, &serde_json::json!({ "role": role }))?;
        Ok(team_detail_with_audit(envelope))
    }

    fn create_stack(
        &self,
        org: &str,
        slug: &str,
        name: &str,
        description: &str,
        visibility: Visibility,
        team: Option<&str>,
    ) -> Result<StackDetail> {
        let path = format!("/orgs/{}/stacks", encode_path_segment(org));
        let envelope: StackEnvelope = self.send_json_body(
            Method::POST,
            &path,
            &serde_json::json!({
                "slug": slug,
                "name": name,
                "description": description,
                "visibility": visibility,
                "team": team,
            }),
        )?;
        Ok(stack_detail_with_audit(envelope))
    }

    fn list_stacks(&self, org: &str) -> Result<Vec<StackSummary>> {
        self.list_stacks_with_filters(org, &StackListFilters::default())
    }

    fn list_stacks_with_filters(
        &self,
        org: &str,
        filters: &StackListFilters,
    ) -> Result<Vec<StackSummary>> {
        let mut path = format!("/orgs/{}/stacks", encode_path_segment(org));
        let mut first = true;
        if let Some(owner) = filters.owner.as_deref() {
            append_query_param(&mut path, &mut first, "owner", owner);
        }
        if let Some(team) = filters.team.as_deref() {
            append_query_param(&mut path, &mut first, "team", team);
        }
        if let Some(limit) = filters.limit {
            append_query_param(&mut path, &mut first, "limit", &limit.to_string());
        }
        let envelope: StackListEnvelope = self.get_json(&path, true)?;
        Ok(envelope.stacks)
    }

    fn inspect_stack(&self, org: &str, stack: &str) -> Result<StackDetail> {
        let path = format!(
            "/orgs/{}/stacks/{}",
            encode_path_segment(org),
            encode_path_segment(stack)
        );
        let envelope: StackEnvelope = self.get_json(&path, true)?;
        Ok(stack_detail_with_audit(envelope))
    }

    fn upsert_stack_item(
        &self,
        org: &str,
        stack: &str,
        skill: &str,
        version_policy: VersionPolicy,
        pinned_version: Option<&str>,
    ) -> Result<StackDetail> {
        let path = format!(
            "/orgs/{}/stacks/{}/items",
            encode_path_segment(org),
            encode_path_segment(stack)
        );
        let envelope: StackEnvelope = self.send_json_body(
            Method::POST,
            &path,
            &serde_json::json!({
                "skill": skill,
                "version_policy": version_policy,
                "pinned_version": pinned_version,
            }),
        )?;
        Ok(stack_detail_with_audit(envelope))
    }

    fn remove_stack_item(&self, org: &str, stack: &str, skill: &str) -> Result<StackDetail> {
        let path = format!(
            "/orgs/{}/stacks/{}/items/{}",
            encode_path_segment(org),
            encode_path_segment(stack),
            encode_path_segment(skill)
        );
        let envelope: StackEnvelope = self.send_json(Method::DELETE, &path)?;
        Ok(stack_detail_with_audit(envelope))
    }

    fn resolve_stack(&self, org: &str, stack: &str) -> Result<StackResolve> {
        let path = format!(
            "/orgs/{}/stacks/{}/resolve",
            encode_path_segment(org),
            encode_path_segment(stack)
        );
        self.get_json(&path, true)
    }

    fn set_stack_visibility(
        &self,
        org: &str,
        stack: &str,
        visibility: Visibility,
        team: Option<&str>,
    ) -> Result<StackDetail> {
        let path = format!(
            "/orgs/{}/stacks/{}/visibility",
            encode_path_segment(org),
            encode_path_segment(stack)
        );
        let envelope: StackEnvelope = self.send_json_body(
            Method::PATCH,
            &path,
            &serde_json::json!({ "visibility": visibility, "team": team }),
        )?;
        Ok(stack_detail_with_audit(envelope))
    }

    fn stack_status(&self, org: &str, stack: &str) -> Result<StackStatus> {
        let path = format!(
            "/orgs/{}/stacks/{}/status",
            encode_path_segment(org),
            encode_path_segment(stack)
        );
        self.get_json(&path, true)
    }

    fn stack_audit(&self, org: &str, stack: &str) -> Result<Vec<AuditEvent>> {
        let path = format!(
            "/orgs/{}/stacks/{}/audit",
            encode_path_segment(org),
            encode_path_segment(stack)
        );
        let envelope: AuditListEnvelope = self.get_json(&path, true)?;
        Ok(envelope.events)
    }

    fn org_audit(&self, org: &str) -> Result<Vec<AuditEvent>> {
        let path = format!("/orgs/{}/audit", encode_path_segment(org));
        let envelope: AuditListEnvelope = self.get_json(&path, true)?;
        Ok(envelope.events)
    }

    fn org_audit_event(&self, org: &str, event_id: &str) -> Result<AuditEvent> {
        let path = format!(
            "/orgs/{}/audit/{}",
            encode_path_segment(org),
            encode_path_segment(event_id)
        );
        let envelope: AuditEventEnvelope = self.get_json(&path, true)?;
        Ok(envelope.event)
    }
}

#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    code: String,
    message: String,
}

#[derive(Debug, Deserialize)]
struct SearchEnvelope {
    results: Vec<SearchResult>,
}

#[derive(Debug, Deserialize)]
struct SkillListEnvelope {
    skills: Vec<RemoteSkill>,
}

#[derive(Debug, Deserialize)]
struct VersionListEnvelope {
    versions: Vec<VersionInfo>,
}

#[derive(Debug, Deserialize)]
struct AuditListEnvelope {
    events: Vec<AuditEvent>,
}

#[derive(Debug, Deserialize)]
struct AuditEventEnvelope {
    event: AuditEvent,
}

#[derive(Debug, Deserialize)]
struct TeamListEnvelope {
    teams: Vec<TeamSummary>,
}

#[derive(Debug, Deserialize)]
struct TeamEnvelope {
    team: TeamDetail,
    #[serde(default)]
    audit_event_id: Option<String>,
}

fn team_detail_with_audit(envelope: TeamEnvelope) -> TeamDetail {
    let mut team = envelope.team;
    team.audit_event_id = envelope.audit_event_id;
    team
}

#[derive(Debug, Deserialize)]
struct StackListEnvelope {
    stacks: Vec<StackSummary>,
}

#[derive(Debug, Deserialize)]
struct StackEnvelope {
    stack: StackDetail,
    #[serde(default)]
    audit_event_id: Option<String>,
}

fn stack_detail_with_audit(envelope: StackEnvelope) -> StackDetail {
    let mut stack = envelope.stack;
    stack.audit_event_id = envelope.audit_event_id;
    stack
}

fn response_error(response: Response, active_token: Option<&Token>) -> anyhow::Error {
    let status = response.status();
    let url = response.url().clone();
    let body = response.text().unwrap_or_default();
    // (code, sanitized message): `Some(code)` when the body is a documented
    // error envelope, `None` for arbitrary non-envelope bodies.
    let (code, server_message) = match serde_json::from_str::<ErrorEnvelope>(&body) {
        Ok(envelope) => (
            Some(stable_registry_error_code(&envelope.error.code)),
            sanitize_registry_error_text(&envelope.error.message, active_token),
        ),
        Err(_) => (None, sanitize_registry_error_text(&body, active_token)),
    };
    let hint = status_hint(status, code.as_deref(), &url);
    let suffix = hint.map(|h| format!("\n  hint: {h}")).unwrap_or_default();
    let message = match code.as_deref() {
        Some(code) => format!(
            "registry request to {url} failed with {status}: {code}: {server_message}{suffix}"
        ),
        None if server_message.trim().is_empty() => {
            format!("registry request to {url} failed with {status}{suffix}")
        }
        None => format!("registry request to {url} failed with {status}: {server_message}{suffix}"),
    };
    let mut error = CliError::new(code.as_deref().unwrap_or("registry_http_error"), message)
        .resource(resource_from_url(&url).unwrap_or_else(|| url.to_string()))
        .action(action_from_url(&url))
        .http_status(status.as_u16());
    if let Some(next_command) = status_next_command(status, code.as_deref(), &url) {
        error = error.next_command(next_command);
    }
    if status.as_u16() == 401 {
        error = error
            .machine_hint("set AGENTSTACK_TOKEN_PATH or AGENTSTACK_TOKEN for automation")
            .auth_methods(["auth_login", "AGENTSTACK_TOKEN_PATH", "AGENTSTACK_TOKEN"]);
    }
    error.into()
}

fn stable_registry_error_code(code: &str) -> String {
    match code {
        "bad_request"
        | "validation_error"
        | "hash_mismatch"
        | "visibility_mismatch"
        | "unauthenticated"
        | "forbidden"
        | "skill_not_found"
        | "team_not_found"
        | "version_not_found"
        | "stack_not_found"
        | "audit_event_not_found"
        | "no_current_version"
        | "quota_exceeded"
        | "stack_resolution_failed"
        | "already_yanked"
        | "already_deprecated"
        | "version_yanked"
        | "payload_too_large"
        | "audit_failed"
        | "oauth_denied"
        | "oauth_expired"
        | "oauth_invalid_grant"
        | "invite_required"
        | "internal_error" => code.to_string(),
        _ => "registry_error".to_string(),
    }
}

fn sanitize_registry_error_text(input: &str, active_token: Option<&Token>) -> String {
    const MAX_CHARS: usize = 512;
    let mut sanitized = input.to_string();
    if let Some(token) = active_token {
        let secret = token.expose_secret();
        if !secret.is_empty() {
            sanitized = sanitized.replace(secret, "[REDACTED]");
        }
    }
    redact_bearer_values(&mut sanitized);
    for key in [
        "authorization",
        "token",
        "access_token",
        "refresh_token",
        "code",
        "code_verifier",
        "code_challenge",
        "state",
        "api_key",
        "apikey",
        "api-key",
    ] {
        redact_assignment_values(&mut sanitized, key);
    }
    truncate_chars(&sanitized, MAX_CHARS)
}

fn redact_bearer_values(text: &mut String) {
    let mut search_from = 0;
    loop {
        let lower = text[search_from..].to_ascii_lowercase();
        let Some(relative) = lower.find("bearer ") else {
            break;
        };
        let value_start = search_from + relative + "bearer ".len();
        let value_end = text[value_start..]
            .char_indices()
            .find(|(_, ch)| ch.is_whitespace() || matches!(ch, '"' | '\'' | ',' | ';'))
            .map(|(idx, _)| value_start + idx)
            .unwrap_or_else(|| text.len());
        if value_end > value_start {
            text.replace_range(value_start..value_end, "[REDACTED]");
            search_from = value_start + "[REDACTED]".len();
        } else {
            search_from = value_start;
        }
    }
}

fn redact_assignment_values(text: &mut String, key: &str) {
    let mut search_from = 0;
    loop {
        let lower = text[search_from..].to_ascii_lowercase();
        let Some(relative) = lower.find(key) else {
            break;
        };
        let key_end = search_from + relative + key.len();
        let Some((value_start, value_end)) = assignment_value_range(text, key_end) else {
            search_from = key_end;
            continue;
        };
        text.replace_range(value_start..value_end, "[REDACTED]");
        search_from = value_start + "[REDACTED]".len();
    }
}

fn assignment_value_range(text: &str, key_end: usize) -> Option<(usize, usize)> {
    let mut cursor = key_end;
    while let Some((idx, ch)) = text[cursor..].char_indices().next() {
        if ch.is_whitespace() || ch == '"' || ch == '\'' {
            cursor += idx + ch.len_utf8();
            continue;
        }
        break;
    }
    let separator = text[cursor..].chars().next()?;
    if !matches!(separator, ':' | '=') {
        return None;
    }
    cursor += separator.len_utf8();
    while let Some((idx, ch)) = text[cursor..].char_indices().next() {
        if ch.is_whitespace() {
            cursor += idx + ch.len_utf8();
            continue;
        }
        break;
    }
    let quote = text[cursor..]
        .chars()
        .next()
        .filter(|ch| matches!(ch, '"' | '\''));
    let value_start = if let Some(quote) = quote {
        cursor + quote.len_utf8()
    } else {
        cursor
    };
    let value_end = text[value_start..]
        .char_indices()
        .find(|(_, ch)| {
            if let Some(quote) = quote {
                *ch == quote
            } else {
                ch.is_whitespace() || matches!(ch, ',' | '&' | ';' | '"' | '\'')
            }
        })
        .map(|(idx, _)| value_start + idx)
        .unwrap_or_else(|| text.len());
    (value_end > value_start).then_some((value_start, value_end))
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let mut output: String = input.chars().take(max_chars).collect();
    output.push_str("... [truncated]");
    output
}

/// Map common HTTP failure statuses to an actionable next-step hint.
///
/// Returned as a suffix on registry error messages so agents and humans get
/// consistent guidance regardless of which command made the request.
fn status_hint(status: reqwest::StatusCode, error_code: Option<&str>, url: &Url) -> Option<String> {
    if error_code == Some("no_current_version") {
        return Some(no_current_version_hint(url));
    }
    if status.as_u16() == 403
        && error_code == Some("forbidden")
        && is_yanked_archive_recovery_url(url)
    {
        return Some("only server admins can recover yanked archives".to_string());
    }
    match status.as_u16() {
        401 => Some(str::to_string(
            "not authenticated — run `agentstack auth login` (or check that your token is still valid)",
        )),
        403 => Some(str::to_string(
            "permission denied — check your role with `agentstack auth whoami`; you may lack the required org role or the resource may not be visible to you",
        )),
        404 => Some(not_found_hint(error_code, url)),
        408 | 429 => Some("registry is rate-limiting or timing out; retry shortly".to_string()),
        500..=599 => Some(str::to_string(
            "registry-side error; retry, then contact your registry admin if it persists",
        )),
        _ => None,
    }
}

fn is_yanked_archive_recovery_url(url: &Url) -> bool {
    url.path().ends_with("/archive")
        && url
            .query_pairs()
            .any(|(key, value)| key == "allow_yanked" && value == "true")
}

fn not_found_hint(error_code: Option<&str>, url: &Url) -> String {
    match error_code {
        Some("stack_not_found") => stack_not_found_hint(url),
        Some("team_not_found") => team_not_found_hint(url),
        Some("audit_event_not_found") => audit_event_not_found_hint(url),
        _ if org_from_url(url, "teams").is_some() => team_not_found_hint(url),
        _ => {
            "not found — confirm the skill ref and version with `agentstack skill search` or `agentstack skill version list`".to_string()
        }
    }
}

fn status_next_command(
    status: reqwest::StatusCode,
    error_code: Option<&str>,
    url: &Url,
) -> Option<String> {
    if error_code == Some("no_current_version") {
        if let Some((org, skill)) = org_skill_from_url(url) {
            return Some(format!(
                "agentstack skill version approve {org}/{skill}@<VERSION>"
            ));
        }
        return Some("agentstack skill version approve <org>/<skill>@<VERSION>".to_string());
    }
    match status.as_u16() {
        401 => Some("agentstack auth login".to_string()),
        404 if org_from_url(url, "teams").is_some() => org_from_url(url, "teams")
            .map(|org| format!("agentstack team list --org {org}"))
            .or_else(|| Some("agentstack team list --org <org>".to_string())),
        404 => match error_code {
            Some("stack_not_found") => org_from_url(url, "stacks")
                .map(|org| format!("agentstack stack list --org {org}"))
                .or_else(|| Some("agentstack stack list --org <org>".to_string())),
            Some("audit_event_not_found") => org_from_url(url, "audit")
                .map(|org| format!("agentstack audit list --org {org}"))
                .or_else(|| Some("agentstack audit list --org <org>".to_string())),
            _ => org_skill_from_url(url)
                .map(|(org, skill)| format!("agentstack skill search {skill} --org {org}"))
                .or_else(|| Some("agentstack skill search <query>".to_string())),
        },
        500..=599 => Some("agentstack registry ping".to_string()),
        _ => None,
    }
}

fn resource_from_url(url: &Url) -> Option<String> {
    let segments: Vec<_> = url.path_segments()?.collect();
    let org_index = segments.iter().position(|segment| *segment == "orgs")?;
    let org = *segments.get(org_index + 1)?;
    match *segments.get(org_index + 2)? {
        "skills" => {
            let skill = *segments.get(org_index + 3)?;
            if *segments.get(org_index + 4).unwrap_or(&"") == "versions" {
                if let Some(version) = segments.get(org_index + 5) {
                    Some(format!("{org}/{skill}@{version}"))
                } else {
                    Some(format!("{org}/{skill}"))
                }
            } else {
                Some(format!("{org}/{skill}"))
            }
        }
        "teams" => {
            let team = *segments.get(org_index + 3)?;
            Some(format!("{org}/{team}"))
        }
        "stacks" => {
            let stack = *segments.get(org_index + 3)?;
            Some(format!("{org}/{stack}"))
        }
        "audit" => segments
            .get(org_index + 3)
            .map(|event_id| format!("{org}/audit/{event_id}"))
            .or_else(|| Some(format!("{org}/audit"))),
        _ => Some(org.to_string()),
    }
}

fn action_from_url(url: &Url) -> &'static str {
    let path = url.path();
    if path.ends_with("/approve") {
        "approve"
    } else if path.ends_with("/yank") {
        "yank"
    } else if path.ends_with("/deprecate") {
        "deprecate"
    } else if path.ends_with("/visibility") {
        "set_visibility"
    } else if path.contains("/archive") {
        "download_archive"
    } else if path.contains("/audit") {
        "audit"
    } else if path.contains("/teams") {
        "team"
    } else if path.contains("/search") {
        "search"
    } else {
        "registry_request"
    }
}

fn no_current_version_hint(url: &Url) -> String {
    if let Some((org, skill)) = org_skill_from_url(url) {
        format!(
            "no approved current version yet; ask an admin to run: agentstack skill version approve {org}/{skill}@<VERSION>"
        )
    } else {
        "no approved current version yet; ask an admin to run: agentstack skill version approve <org>/<skill>@<VERSION>"
            .to_string()
    }
}

fn stack_not_found_hint(url: &Url) -> String {
    if let Some(org) = org_from_url(url, "stacks") {
        format!(
            "not found or not visible — run `agentstack stack list --org {org}` and verify stack access"
        )
    } else {
        "not found or not visible — run `agentstack stack list --org <org>` and verify stack access"
            .to_string()
    }
}

fn team_not_found_hint(url: &Url) -> String {
    if let Some(org) = org_from_url(url, "teams") {
        format!(
            "not found or not visible — run `agentstack team list --org {org}` and verify team access"
        )
    } else {
        "not found or not visible — run `agentstack team list --org <org>` and verify team access"
            .to_string()
    }
}

fn audit_event_not_found_hint(url: &Url) -> String {
    org_from_url(url, "audit")
        .map(|org| {
            format!(
                "not found — confirm the audit event id with `agentstack audit list --org {org}`"
            )
        })
        .unwrap_or_else(|| {
            "not found — confirm the audit event id with `agentstack audit list --org <org>`"
                .to_string()
        })
}

/// Extract the org slug from `/orgs/{org}/{section}/...` URLs, e.g.
/// `org_from_url(url, "stacks")` for stack endpoints.
fn org_from_url<'a>(url: &'a Url, section: &str) -> Option<&'a str> {
    let mut segments = url.path_segments()?;
    while let Some(segment) = segments.next() {
        if segment == "orgs" {
            let org = segments.next()?;
            if segments.next()? == section {
                return Some(org);
            }
        }
    }
    None
}

fn org_skill_from_url(url: &Url) -> Option<(&str, &str)> {
    let mut segments = url.path_segments()?;
    while let Some(segment) = segments.next() {
        if segment == "orgs" {
            let org = segments.next()?;
            if segments.next()? == "skills" {
                let skill = segments.next()?;
                return Some((org, skill));
            }
        }
    }
    None
}

/// Parsed registry base URL with API endpoint construction rules.
#[derive(Debug, Clone)]
pub struct RegistryUrl {
    api_base: Url,
}

impl RegistryUrl {
    pub fn parse(input: &str) -> std::result::Result<Self, RegistryUrlError> {
        if input.is_empty() {
            return Err(RegistryUrlError::Empty);
        }
        if input.trim() != input {
            return Err(RegistryUrlError::SurroundingWhitespace);
        }
        if !has_http_scheme_prefix(input) {
            return Err(RegistryUrlError::UnsupportedScheme);
        }
        if raw_authority(input).is_some_and(str::is_empty) {
            return Err(RegistryUrlError::MissingHost);
        }
        if raw_host_has_whitespace(input) {
            return Err(RegistryUrlError::HostWhitespace);
        }

        let parsed = Url::parse(input).map_err(map_url_parse_error)?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(RegistryUrlError::UnsupportedScheme);
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(RegistryUrlError::QueryOrFragment);
        }
        let host = parsed.host().ok_or(RegistryUrlError::MissingHost)?;
        if parsed.scheme() == "http" && !is_loopback_registry_host(&host) {
            return Err(RegistryUrlError::InsecureScheme);
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(RegistryUrlError::UserInfo);
        }

        Ok(Self {
            api_base: normalize_api_base(parsed),
        })
    }

    pub fn endpoint(&self, path: &str) -> std::result::Result<Url, RegistryUrlError> {
        let relative = path.strip_prefix('/').unwrap_or(path);
        self.api_base
            .join(relative)
            .map_err(|err| RegistryUrlError::Endpoint {
                reason: err.to_string(),
            })
    }

    pub fn normalized_base(&self) -> &Url {
        &self.api_base
    }
}

/// Errors produced while parsing or using a registry URL.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RegistryUrlError {
    #[error("registry URL must not be empty")]
    Empty,
    #[error("registry URL must not have leading or trailing whitespace")]
    SurroundingWhitespace,
    #[error("registry URL must start with http:// or https://")]
    UnsupportedScheme,
    #[error(
        "registry URL must use https:// unless it points at a loopback host (localhost, 127.0.0.1, ::1)"
    )]
    InsecureScheme,
    #[error("registry URL must be a base URL without query or fragment")]
    QueryOrFragment,
    #[error("registry URL must include a host")]
    MissingHost,
    #[error("registry URL must not include username or password")]
    UserInfo,
    #[error("registry URL host must not contain whitespace")]
    HostWhitespace,
    #[error("registry URL is not valid: {reason}")]
    Invalid { reason: String },
    #[error("failed to build registry endpoint URL: {reason}")]
    Endpoint { reason: String },
}

fn is_loopback_registry_host(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Domain(name) => name.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(ip) => ip.is_loopback(),
        url::Host::Ipv6(ip) => ip.is_loopback(),
    }
}

fn has_http_scheme_prefix(input: &str) -> bool {
    input
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
        || input
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
}

fn raw_host_has_whitespace(input: &str) -> bool {
    raw_authority(input)
        .map(|authority| {
            authority
                .rsplit('@')
                .next()
                .is_some_and(|host| host.chars().any(char::is_whitespace))
        })
        .unwrap_or(false)
}

fn raw_authority(input: &str) -> Option<&str> {
    input
        .split_once("://")
        .and_then(|(_, rest)| rest.split(['/', '?', '#']).next())
}

fn map_url_parse_error(err: url::ParseError) -> RegistryUrlError {
    match err {
        url::ParseError::EmptyHost => RegistryUrlError::MissingHost,
        err => RegistryUrlError::Invalid {
            reason: err.to_string(),
        },
    }
}

fn normalize_api_base(mut parsed: Url) -> Url {
    let trimmed_path = parsed.path().trim_end_matches('/');
    let api_path = if trimmed_path.ends_with("/v1") {
        format!("{trimmed_path}/")
    } else if trimmed_path.is_empty() {
        "/v1/".to_string()
    } else {
        format!("{trimmed_path}/v1/")
    };
    parsed.set_path(&api_path);
    parsed
}

fn encode_path_segment(value: &str) -> String {
    percent_encode(value, false)
}

fn encode_query_component(value: &str) -> String {
    percent_encode(value, true)
}

fn append_query_param(path: &mut String, first: &mut bool, key: &str, value: &str) {
    path.push(if *first { '?' } else { '&' });
    *first = false;
    path.push_str(key);
    path.push('=');
    path.push_str(&encode_query_component(value));
}

fn percent_encode(value: &str, space_as_plus: bool) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if is_unreserved(byte) {
            out.push(byte as char);
        } else if byte == b' ' && space_as_plus {
            out.push('+');
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    out
}

fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

/// In-memory registry used by tests. Push/pull/search/list/versions
/// round-trip through a `Mutex<MockState>`. Ping/whoami return canned
/// successes unless `ping` is explicitly failed via
/// [`MockRegistryClient::fail_next_ping`].
#[derive(Debug, Default)]
pub struct MockRegistryClient {
    state: Mutex<MockState>,
}

#[derive(Debug, Default)]
struct MockState {
    user: Option<WhoamiResponse>,
    server_version: Option<String>,
    /// (org, name, version) -> archive bytes
    artifacts: BTreeMap<(String, String, String), Vec<u8>>,
    /// (org, name, version) -> stored metadata (with timestamps)
    metadata: BTreeMap<(String, String, String), SkillMetadata>,
    /// (org, name) -> newest uploaded version for that skill
    latest: BTreeMap<(String, String), String>,
    /// (org, name) -> current approved version for that skill
    current: BTreeMap<(String, String), String>,
    /// (org, stack) -> stack definition
    stacks: BTreeMap<(String, String), MockStack>,
    push_calls: usize,
    pull_calls: usize,
    resolve_stack_calls: usize,
    list_versions_calls: usize,
    next_ping_error: Option<String>,
    next_push_error: Option<String>,
    next_list_versions_error: Option<String>,
}

#[derive(Debug, Clone)]
struct MockStack {
    org: String,
    slug: String,
    name: String,
    description: String,
    owner_email: Option<String>,
    visibility: Visibility,
    team: Option<String>,
    created_at: String,
    updated_at: String,
    items: Vec<MockStackItem>,
}

#[derive(Debug, Clone)]
struct MockStackItem {
    skill: String,
    version_policy: VersionPolicy,
    pinned_version: Option<String>,
    position: i64,
    added_at: String,
}

impl MockRegistryClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_user(user: impl Into<String>) -> Self {
        let s = Self::new();
        let user = user.into();
        s.set_user(WhoamiResponse {
            user: user.clone(),
            org: None,
            email: user,
            name: None,
            server_admin: false,
            orgs: vec![],
        });
        s
    }

    pub fn set_user(&self, user: WhoamiResponse) {
        self.state.lock().unwrap().user = Some(user);
    }

    pub fn set_server_version(&self, v: impl Into<String>) {
        self.state.lock().unwrap().server_version = Some(v.into());
    }

    /// Cause the next `ping` call to fail with the given message.
    pub fn fail_next_ping(&self, msg: impl Into<String>) {
        self.state.lock().unwrap().next_ping_error = Some(msg.into());
    }

    /// Cause the next `push` call to fail with the given message.
    pub fn fail_next_push(&self, msg: impl Into<String>) {
        self.state.lock().unwrap().next_push_error = Some(msg.into());
    }

    /// Cause the next `list_versions` call to fail with the given message.
    pub fn fail_next_list_versions(&self, msg: impl Into<String>) {
        self.state.lock().unwrap().next_list_versions_error = Some(msg.into());
    }

    /// Test-only: count attempted push calls.
    pub fn push_count(&self) -> usize {
        self.state.lock().unwrap().push_calls
    }

    /// Test-only: count attempted pull calls.
    pub fn pull_count(&self) -> usize {
        self.state.lock().unwrap().pull_calls
    }

    /// Test-only: count attempted stack resolve calls.
    pub fn resolve_stack_count(&self) -> usize {
        self.state.lock().unwrap().resolve_stack_calls
    }

    /// Test-only: count attempted `list_versions` calls.
    pub fn list_versions_count(&self) -> usize {
        self.state.lock().unwrap().list_versions_calls
    }

    /// Test-only: peek at a stored artifact.
    pub fn pushed(&self, org: &str, name: &str, version: &str) -> Option<Vec<u8>> {
        self.state
            .lock()
            .unwrap()
            .artifacts
            .get(&(org.to_string(), name.to_string(), version.to_string()))
            .cloned()
    }

    /// Test-only: peek at stored metadata.
    pub fn pushed_metadata(&self, org: &str, name: &str, version: &str) -> Option<SkillMetadata> {
        self.state
            .lock()
            .unwrap()
            .metadata
            .get(&(org.to_string(), name.to_string(), version.to_string()))
            .cloned()
    }

    /// Test-only: directly seed an entry without exercising `push`.
    pub fn seed(&self, metadata: SkillMetadata, archive: Vec<u8>) {
        let key = (
            metadata.org.clone(),
            metadata.name.clone(),
            metadata.version.clone(),
        );
        let mut s = self.state.lock().unwrap();
        s.latest.insert(
            (metadata.org.clone(), metadata.name.clone()),
            metadata.version.clone(),
        );
        if metadata.current == Some(true) {
            s.current.insert(
                (metadata.org.clone(), metadata.name.clone()),
                metadata.version.clone(),
            );
        }
        s.metadata.insert(key.clone(), metadata);
        s.artifacts.insert(key, archive);
    }
}

impl RegistryClient for MockRegistryClient {
    fn ping(&self) -> Result<PingResponse> {
        let mut s = self.state.lock().unwrap();
        if let Some(err) = s.next_ping_error.take() {
            return Err(anyhow!(err));
        }
        Ok(PingResponse {
            status: "ok".to_string(),
            server_version: s
                .server_version
                .clone()
                .unwrap_or_else(|| "mock".to_string()),
        })
    }

    fn whoami(&self) -> Result<WhoamiResponse> {
        let s = self.state.lock().unwrap();
        s.user
            .clone()
            .ok_or_else(|| anyhow!("mock has no configured user"))
    }

    fn push(&self, request: PushRequest<'_>) -> Result<PushResponse> {
        let mut s = self.state.lock().unwrap();
        s.push_calls += 1;
        if let Some(err) = s.next_push_error.take() {
            return Err(anyhow!(err));
        }
        let mut metadata = request.metadata;
        // Fill in deterministic timestamps so tests can compare full metadata.
        metadata
            .created_at
            .get_or_insert_with(|| "2026-01-01T00:00:00Z".to_string());
        metadata.updated_at = Some("2026-01-01T00:00:00Z".to_string());
        metadata.status = Some(VersionStatus::Candidate);
        metadata.current = Some(false);
        metadata.owner_email.get_or_insert_with(|| {
            s.user
                .as_ref()
                .map(|user| user.email.clone())
                .unwrap_or_else(|| "mock@example.com".to_string())
        });
        metadata.install_count = None;
        metadata.last_installed_at = None;
        metadata.yanked_at = None;
        metadata.yank_reason = None;
        metadata.deprecated_at = None;
        metadata.deprecation_reason = None;
        metadata.audit_event_id = Some("aud_mock".to_string());
        let next_version = s
            .metadata
            .keys()
            .filter(|(org, name, _)| org == &metadata.org && name == &metadata.name)
            .filter_map(|(_, _, version)| version.parse::<i64>().ok())
            .max()
            .unwrap_or(0)
            + 1;
        metadata.version = next_version.to_string();

        let key = (
            metadata.org.clone(),
            metadata.name.clone(),
            metadata.version.clone(),
        );
        s.latest.insert(
            (metadata.org.clone(), metadata.name.clone()),
            metadata.version.clone(),
        );
        s.artifacts.insert(key.clone(), request.archive.to_vec());
        s.metadata.insert(key, metadata.clone());

        let url = Some(format!(
            "mock://skills/{}/{}/{}",
            metadata.org, metadata.name, metadata.version
        ));
        Ok(PushResponse {
            skill_ref: metadata.skill_ref(),
            version: metadata.version.clone(),
            sha256: metadata.hash.hex.clone(),
            visibility: metadata.visibility,
            metadata,
            url,
            audit_event_id: Some("aud_mock".to_string()),
        })
    }

    fn pull_with_options(
        &self,
        skill_ref: &SkillRef,
        options: PullClientOptions,
    ) -> Result<PullResponse> {
        let mut s = self.state.lock().unwrap();
        s.pull_calls += 1;
        let version = match &skill_ref.version {
            Some(v) => v.clone(),
            None => s
                .current
                .get(&(skill_ref.org.clone(), skill_ref.name.clone()))
                .cloned()
                .ok_or_else(|| {
                    if s.latest
                        .contains_key(&(skill_ref.org.clone(), skill_ref.name.clone()))
                    {
                        anyhow!(
                            "`{}/{}` has uploaded candidate versions but no approved/current version yet; ask an org or team admin to run `agentstack skill version approve {}/{}@<VERSION>`",
                            skill_ref.org,
                            skill_ref.name,
                            skill_ref.org,
                            skill_ref.name
                        )
                    } else {
                        anyhow!("no such skill `{}/{}`", skill_ref.org, skill_ref.name)
                    }
                })?,
        };
        let key = (
            skill_ref.org.clone(),
            skill_ref.name.clone(),
            version.clone(),
        );
        let metadata = s.metadata.get(&key).cloned().ok_or_else(|| {
            anyhow!(
                "no metadata for `{}/{}@{version}`",
                skill_ref.org,
                skill_ref.name
            )
        })?;
        let archive = s.artifacts.get(&key).cloned().ok_or_else(|| {
            anyhow!(
                "no artifact for `{}/{}@{version}`",
                skill_ref.org,
                skill_ref.name
            )
        })?;
        if metadata.yanked_at.is_some() && !options.allow_yanked {
            let reason = metadata
                .yank_reason
                .clone()
                .unwrap_or_else(|| "yanked".to_string());
            bail!(
                "`{}/{}@{}` was yanked: {reason}",
                skill_ref.org,
                skill_ref.name,
                version
            );
        }
        Ok(PullResponse { metadata, archive })
    }

    fn approve(&self, skill_ref: &SkillRef, version: &str) -> Result<SkillMetadata> {
        let mut s = self.state.lock().unwrap();
        let key = (
            skill_ref.org.clone(),
            skill_ref.name.clone(),
            version.to_string(),
        );
        if !s.metadata.contains_key(&key) {
            bail!(
                "no such version `{}/{}@{version}`",
                skill_ref.org,
                skill_ref.name
            );
        }
        let current_key = (skill_ref.org.clone(), skill_ref.name.clone());
        if let Some(previous) = s.current.insert(current_key, version.to_string())
            && previous != version
            && let Some(previous_meta) =
                s.metadata
                    .get_mut(&(skill_ref.org.clone(), skill_ref.name.clone(), previous))
        {
            previous_meta.current = Some(false);
        }
        let metadata = s.metadata.get_mut(&key).expect("checked above");
        metadata.status = Some(VersionStatus::Approved);
        metadata.current = Some(true);
        Ok(metadata.clone())
    }

    fn yank(&self, skill_ref: &SkillRef, version: &str, reason: &str) -> Result<SkillMetadata> {
        let mut s = self.state.lock().unwrap();
        let key = (
            skill_ref.org.clone(),
            skill_ref.name.clone(),
            version.to_string(),
        );
        let metadata = s.metadata.get_mut(&key).ok_or_else(|| {
            anyhow!(
                "no such version `{}/{}@{version}`",
                skill_ref.org,
                skill_ref.name
            )
        })?;
        if metadata.yanked_at.is_some() {
            bail!(
                "`{}/{}@{version}` is already yanked",
                skill_ref.org,
                skill_ref.name
            );
        }
        metadata.yanked_at = Some("2026-01-01T00:00:00Z".to_string());
        metadata.yank_reason = Some(reason.to_string());
        Ok(metadata.clone())
    }

    fn deprecate(
        &self,
        skill_ref: &SkillRef,
        version: &str,
        reason: &str,
    ) -> Result<SkillMetadata> {
        let mut s = self.state.lock().unwrap();
        let key = (
            skill_ref.org.clone(),
            skill_ref.name.clone(),
            version.to_string(),
        );
        let metadata = s.metadata.get_mut(&key).ok_or_else(|| {
            anyhow!(
                "no such version `{}/{}@{version}`",
                skill_ref.org,
                skill_ref.name
            )
        })?;
        if metadata.deprecated_at.is_some() {
            bail!(
                "`{}/{}@{version}` is already deprecated",
                skill_ref.org,
                skill_ref.name
            );
        }
        metadata.deprecated_at = Some("2026-01-01T00:00:00Z".to_string());
        metadata.deprecation_reason = Some(reason.to_string());
        Ok(metadata.clone())
    }

    fn search(&self, query: &str) -> Result<Vec<SearchResult>> {
        self.search_with_filters(query, &SearchFilters::default())
    }

    fn search_with_filters(
        &self,
        query: &str,
        filters: &SearchFilters,
    ) -> Result<Vec<SearchResult>> {
        let s = self.state.lock().unwrap();
        Ok(mock_catalog_rows(&s, Some(query), filters))
    }

    fn list_remote(&self, org: Option<&str>) -> Result<Vec<RemoteSkill>> {
        let filters = SearchFilters {
            org: org.map(str::to_string),
            ..SearchFilters::default()
        };
        self.list_remote_with_filters(&filters)
    }

    fn list_remote_with_filters(&self, filters: &SearchFilters) -> Result<Vec<RemoteSkill>> {
        let s = self.state.lock().unwrap();
        Ok(mock_catalog_rows(&s, None, filters))
    }

    fn list_versions(&self, skill_ref: &SkillRef) -> Result<Vec<VersionInfo>> {
        let mut s = self.state.lock().unwrap();
        s.list_versions_calls += 1;
        if let Some(err) = s.next_list_versions_error.take() {
            return Err(anyhow!(err));
        }
        let mut out: Vec<VersionInfo> = s
            .metadata
            .iter()
            .filter(|((o, n, _), _)| o == &skill_ref.org && n == &skill_ref.name)
            .map(|(_, meta)| VersionInfo {
                version: meta.version.clone(),
                hash: meta.hash.clone(),
                platform_tags: meta.platform_tags.clone(),
                created_at: meta.created_at.clone(),
                status: meta.status,
                current: meta.current,
                yanked_at: meta.yanked_at.clone(),
                yank_reason: meta.yank_reason.clone(),
                deprecated_at: meta.deprecated_at.clone(),
                deprecation_reason: meta.deprecation_reason.clone(),
            })
            .collect();
        if out.is_empty() {
            bail!("no such skill `{}/{}`", skill_ref.org, skill_ref.name);
        }
        out.sort_by(|a, b| compare_registry_versions(&b.version, &a.version));
        Ok(out)
    }

    fn skill_metadata(&self, skill_ref: &SkillRef) -> Result<SkillMetadata> {
        let s = self.state.lock().unwrap();
        let version = match &skill_ref.version {
            Some(version) => version.clone(),
            None => s
                .current
                .get(&(skill_ref.org.clone(), skill_ref.name.clone()))
                .cloned()
                .ok_or_else(|| {
                    if s.latest
                        .contains_key(&(skill_ref.org.clone(), skill_ref.name.clone()))
                    {
                        anyhow!(
                            "no approved/current version for `{}/{}`",
                            skill_ref.org,
                            skill_ref.name
                        )
                    } else {
                        anyhow!("no such skill `{}/{}`", skill_ref.org, skill_ref.name)
                    }
                })?,
        };
        s.metadata
            .get(&(
                skill_ref.org.clone(),
                skill_ref.name.clone(),
                version.clone(),
            ))
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "no metadata for `{}/{}@{version}`",
                    skill_ref.org,
                    skill_ref.name
                )
            })
    }

    fn skill_status(&self, skill_ref: &SkillRef) -> Result<SkillStatus> {
        let versions = self.list_versions(skill_ref)?;
        let skills = self.list_remote(Some(&skill_ref.org))?;
        let skill = skills
            .into_iter()
            .find(|candidate| candidate.name == skill_ref.name)
            .ok_or_else(|| anyhow!("no such skill `{}/{}`", skill_ref.org, skill_ref.name))?;
        Ok(SkillStatus { skill, versions })
    }

    fn skill_impact(&self, skill_ref: &SkillRef) -> Result<SkillImpact> {
        let status = self.skill_status(skill_ref)?;
        let s = self.state.lock().unwrap();
        let mut used_by = Vec::new();
        for stack in s.stacks.values() {
            if stack.org != skill_ref.org {
                continue;
            }
            for item in stack
                .items
                .iter()
                .filter(|item| item.skill == skill_ref.name)
            {
                let effective_version = match item.version_policy {
                    VersionPolicy::Current => status.skill.current_version.clone(),
                    VersionPolicy::Pinned => item.pinned_version.clone(),
                };
                let version = effective_version.as_ref().and_then(|version| {
                    s.metadata.get(&(
                        skill_ref.org.clone(),
                        skill_ref.name.clone(),
                        version.clone(),
                    ))
                });
                used_by.push(SkillImpactStack {
                    stack: format!("{}/{}", stack.org, stack.slug),
                    org: stack.org.clone(),
                    slug: stack.slug.clone(),
                    name: stack.name.clone(),
                    owner_email: stack.owner_email.clone(),
                    visibility: stack.visibility,
                    team: stack.team.clone(),
                    version_policy: item.version_policy,
                    pinned_version: item.pinned_version.clone(),
                    effective_version,
                    status: version.and_then(|entry| entry.status),
                    current: version.and_then(|entry| entry.current).unwrap_or(false),
                    yanked_at: version.and_then(|entry| entry.yanked_at.clone()),
                    yank_reason: version.and_then(|entry| entry.yank_reason.clone()),
                    deprecated_at: version.and_then(|entry| entry.deprecated_at.clone()),
                    deprecation_reason: version.and_then(|entry| entry.deprecation_reason.clone()),
                });
            }
        }
        used_by.sort_by(|a, b| a.stack.cmp(&b.stack));
        let current_policy_count = used_by
            .iter()
            .filter(|stack| stack.version_policy == VersionPolicy::Current)
            .count();
        let pinned_count = used_by
            .iter()
            .filter(|stack| stack.version_policy == VersionPolicy::Pinned)
            .count();
        Ok(SkillImpact {
            skill: status.skill,
            summary: SkillImpactSummary {
                used_by_count: used_by.len(),
                current_policy_count,
                pinned_count,
                visible_only: true,
            },
            used_by,
        })
    }

    fn skill_audit(&self, _skill_ref: &SkillRef) -> Result<Vec<AuditEvent>> {
        Ok(Vec::new())
    }

    fn skill_visibility(&self, skill_ref: &SkillRef) -> Result<VisibilityStatus> {
        let s = self.state.lock().unwrap();
        let latest = s
            .latest
            .get(&(skill_ref.org.clone(), skill_ref.name.clone()))
            .ok_or_else(|| anyhow!("no such skill `{}/{}`", skill_ref.org, skill_ref.name))?;
        let metadata = s
            .metadata
            .get(&(
                skill_ref.org.clone(),
                skill_ref.name.clone(),
                latest.clone(),
            ))
            .ok_or_else(|| anyhow!("no such skill `{}/{}`", skill_ref.org, skill_ref.name))?;
        Ok(VisibilityStatus {
            org: skill_ref.org.clone(),
            skill: skill_ref.name.clone(),
            visibility: metadata.visibility,
            team: metadata.team.clone(),
            audit_event_id: None,
        })
    }

    fn set_skill_visibility(
        &self,
        skill_ref: &SkillRef,
        visibility: Visibility,
        team: Option<&str>,
    ) -> Result<VisibilityStatus> {
        if visibility == Visibility::Team && team.is_none() {
            bail!("--team is required when --scope team is used");
        }
        if visibility != Visibility::Team && team.is_some() {
            bail!("--team can only be used with --scope team");
        }
        let mut s = self.state.lock().unwrap();
        let mut found = false;
        for ((org, name, _), metadata) in s.metadata.iter_mut() {
            if org == &skill_ref.org && name == &skill_ref.name {
                metadata.visibility = visibility;
                metadata.team = team.map(str::to_string);
                found = true;
            }
        }
        if !found {
            bail!("no such skill `{}/{}`", skill_ref.org, skill_ref.name);
        }
        Ok(VisibilityStatus {
            org: skill_ref.org.clone(),
            skill: skill_ref.name.clone(),
            visibility,
            team: team.map(str::to_string),
            audit_event_id: Some("aud_mock".to_string()),
        })
    }

    fn create_stack(
        &self,
        org: &str,
        slug: &str,
        name: &str,
        description: &str,
        visibility: Visibility,
        team: Option<&str>,
    ) -> Result<StackDetail> {
        let mut s = self.state.lock().unwrap();
        let key = (org.to_string(), slug.to_string());
        if s.stacks.contains_key(&key) {
            bail!("stack `{org}/{slug}` already exists");
        }
        let stack = MockStack {
            org: org.to_string(),
            slug: slug.to_string(),
            name: name.to_string(),
            description: description.to_string(),
            owner_email: None,
            visibility,
            team: team.map(str::to_string),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            items: Vec::new(),
        };
        let detail = with_mock_audit(stack_detail_from_mock(&stack));
        s.stacks.insert(key, stack);
        Ok(detail)
    }

    fn list_stacks(&self, org: &str) -> Result<Vec<StackSummary>> {
        self.list_stacks_with_filters(org, &StackListFilters::default())
    }

    fn list_stacks_with_filters(
        &self,
        org: &str,
        filters: &StackListFilters,
    ) -> Result<Vec<StackSummary>> {
        let s = self.state.lock().unwrap();
        let mut out: Vec<StackSummary> = s
            .stacks
            .iter()
            .filter(|((o, _), stack)| {
                o == org
                    && filters
                        .owner
                        .as_deref()
                        .is_none_or(|owner| stack.owner_email.as_deref() == Some(owner))
                    && filters
                        .team
                        .as_deref()
                        .is_none_or(|team| stack.team.as_deref() == Some(team))
            })
            .map(|(_, stack)| stack_summary_from_mock(stack))
            .collect();
        out.sort_by(|a, b| a.slug.cmp(&b.slug));
        if let Some(limit) = filters.limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    fn inspect_stack(&self, org: &str, stack: &str) -> Result<StackDetail> {
        let s = self.state.lock().unwrap();
        let stack = s
            .stacks
            .get(&(org.to_string(), stack.to_string()))
            .ok_or_else(|| anyhow!("no such stack `{org}/{stack}`"))?;
        Ok(stack_detail_from_mock(stack))
    }

    fn upsert_stack_item(
        &self,
        org: &str,
        stack: &str,
        skill: &str,
        version_policy: VersionPolicy,
        pinned_version: Option<&str>,
    ) -> Result<StackDetail> {
        let mut s = self.state.lock().unwrap();
        if !s.latest.contains_key(&(org.to_string(), skill.to_string())) {
            bail!("no such skill `{org}/{skill}`");
        }
        if version_policy == VersionPolicy::Pinned {
            let version = pinned_version
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| anyhow!("version is required when version_policy is `pinned`"))?;
            if !s
                .metadata
                .contains_key(&(org.to_string(), skill.to_string(), version.to_string()))
            {
                bail!("no such version `{org}/{skill}@{version}`");
            }
        } else if pinned_version.is_some() {
            bail!("version is only valid when version_policy is `pinned`");
        }

        let key = (org.to_string(), stack.to_string());
        let stack_def = s
            .stacks
            .get_mut(&key)
            .ok_or_else(|| anyhow!("no such stack `{org}/{stack}`"))?;
        let position = stack_def
            .items
            .iter()
            .map(|item| item.position)
            .max()
            .unwrap_or(-1)
            + 1;
        if let Some(existing) = stack_def.items.iter_mut().find(|item| item.skill == skill) {
            existing.version_policy = version_policy;
            existing.pinned_version = pinned_version.map(str::to_string);
        } else {
            stack_def.items.push(MockStackItem {
                skill: skill.to_string(),
                version_policy,
                pinned_version: pinned_version.map(str::to_string),
                position,
                added_at: "2026-01-01T00:00:00Z".to_string(),
            });
        }
        stack_def.updated_at = "2026-01-01T00:00:00Z".to_string();
        Ok(with_mock_audit(stack_detail_from_mock(stack_def)))
    }

    fn remove_stack_item(&self, org: &str, stack: &str, skill: &str) -> Result<StackDetail> {
        let mut s = self.state.lock().unwrap();
        let stack_def = s
            .stacks
            .get_mut(&(org.to_string(), stack.to_string()))
            .ok_or_else(|| anyhow!("no such stack `{org}/{stack}`"))?;
        let before = stack_def.items.len();
        stack_def.items.retain(|item| item.skill != skill);
        if stack_def.items.len() == before {
            bail!("skill `{org}/{skill}` is not in stack `{org}/{stack}`");
        }
        stack_def.updated_at = "2026-01-01T00:00:00Z".to_string();
        Ok(with_mock_audit(stack_detail_from_mock(stack_def)))
    }

    fn resolve_stack(&self, org: &str, stack: &str) -> Result<StackResolve> {
        let mut s = self.state.lock().unwrap();
        s.resolve_stack_calls += 1;
        let stack_def = s
            .stacks
            .get(&(org.to_string(), stack.to_string()))
            .cloned()
            .ok_or_else(|| anyhow!("no such stack `{org}/{stack}`"))?;
        let mut resolved_items = Vec::new();
        for item in &stack_def.items {
            let version = match item.version_policy {
                VersionPolicy::Current => s
                    .current
                    .get(&(org.to_string(), item.skill.clone()))
                    .cloned()
                    .ok_or_else(|| {
                        anyhow!(
                            "stack cannot be resolved because `{}/{}` has no current version",
                            org,
                            item.skill
                        )
                    })?,
                VersionPolicy::Pinned => item
                    .pinned_version
                    .clone()
                    .ok_or_else(|| anyhow!("stack item `{}` has no pinned version", item.skill))?,
            };
            let metadata = s
                .metadata
                .get(&(org.to_string(), item.skill.clone(), version.clone()))
                .ok_or_else(|| anyhow!("no metadata for `{}/{}`", org, item.skill))?;
            if metadata.status != Some(VersionStatus::Approved) || metadata.yanked_at.is_some() {
                bail!("stack cannot be resolved because at least one item is unavailable");
            }
            resolved_items.push(StackResolvedItem {
                skill: item.skill.clone(),
                version_id: format!("ver_{}_{}", item.skill, version),
                version: version.clone(),
                archive_hash: metadata.hash.clone(),
                download: StackDownloadRoute {
                    method: "GET".to_string(),
                    url: format!(
                        "/v1/orgs/{org}/skills/{}/versions/{version}/archive",
                        item.skill
                    ),
                },
                version_policy: item.version_policy,
            });
        }
        let header = StackResolveHeader {
            org: stack_def.org,
            slug: stack_def.slug,
            name: stack_def.name,
            visibility: stack_def.visibility,
            team: stack_def.team,
        };
        let manifest_body = serde_json::json!({
            "stack": &header,
            "items": &resolved_items,
        });
        let manifest_bytes =
            serde_json::to_vec(&manifest_body).context("failed to serialize stack manifest")?;
        Ok(StackResolve {
            stack: header,
            resolved_at: "2026-01-01T00:00:00Z".to_string(),
            manifest_hash: PackageHash::sha256_of(&manifest_bytes),
            items: resolved_items,
        })
    }

    fn set_stack_visibility(
        &self,
        org: &str,
        stack: &str,
        visibility: Visibility,
        team: Option<&str>,
    ) -> Result<StackDetail> {
        if visibility == Visibility::Team && team.is_none() {
            bail!("--team is required when --scope team is used");
        }
        if visibility != Visibility::Team && team.is_some() {
            bail!("--team can only be used with --scope team");
        }
        let mut s = self.state.lock().unwrap();
        let stack_def = s
            .stacks
            .get_mut(&(org.to_string(), stack.to_string()))
            .ok_or_else(|| anyhow!("no such stack `{org}/{stack}`"))?;
        stack_def.visibility = visibility;
        stack_def.team = team.map(str::to_string);
        stack_def.updated_at = "2026-01-01T00:00:00Z".to_string();
        Ok(with_mock_audit(stack_detail_from_mock(stack_def)))
    }

    fn stack_status(&self, org: &str, stack: &str) -> Result<StackStatus> {
        Ok(StackStatus {
            stack: self.inspect_stack(org, stack)?,
        })
    }

    fn stack_audit(&self, _org: &str, _stack: &str) -> Result<Vec<AuditEvent>> {
        Ok(Vec::new())
    }

    fn org_audit(&self, _org: &str) -> Result<Vec<AuditEvent>> {
        Ok(Vec::new())
    }

    fn org_audit_event(&self, org: &str, event_id: &str) -> Result<AuditEvent> {
        self.org_audit(org)?
            .into_iter()
            .find(|event| event.id == event_id)
            .ok_or_else(|| anyhow!("no such audit event `{event_id}`"))
    }
}

fn stack_summary_from_mock(stack: &MockStack) -> StackSummary {
    StackSummary {
        org: stack.org.clone(),
        slug: stack.slug.clone(),
        name: stack.name.clone(),
        description: stack.description.clone(),
        owner_email: stack.owner_email.clone(),
        visibility: stack.visibility,
        team: stack.team.clone(),
        item_count: stack.items.len() as i64,
        created_at: stack.created_at.clone(),
        updated_at: stack.updated_at.clone(),
    }
}

fn stack_detail_from_mock(stack: &MockStack) -> StackDetail {
    let mut items: Vec<StackItemSummary> = stack
        .items
        .iter()
        .map(|item| StackItemSummary {
            skill: item.skill.clone(),
            version_policy: item.version_policy,
            pinned_version: item.pinned_version.clone(),
            position: item.position,
            added_at: item.added_at.clone(),
        })
        .collect();
    items.sort_by(|a, b| {
        a.position
            .cmp(&b.position)
            .then_with(|| a.added_at.cmp(&b.added_at))
            .then_with(|| a.skill.cmp(&b.skill))
    });
    StackDetail {
        org: stack.org.clone(),
        slug: stack.slug.clone(),
        name: stack.name.clone(),
        description: stack.description.clone(),
        owner_email: stack.owner_email.clone(),
        visibility: stack.visibility,
        team: stack.team.clone(),
        created_at: stack.created_at.clone(),
        updated_at: stack.updated_at.clone(),
        items,
        audit_event_id: None,
    }
}

fn with_mock_audit(mut detail: StackDetail) -> StackDetail {
    detail.audit_event_id = Some("aud_mock".to_string());
    detail
}

fn visible_latest_metadata<'a>(
    state: &'a MockState,
    org: &str,
    name: &str,
) -> Option<&'a SkillMetadata> {
    state
        .metadata
        .iter()
        .filter(|((o, n, _), meta)| o == org && n == name && meta.yanked_at.is_none())
        .map(|(_, meta)| meta)
        .max_by(|a, b| compare_registry_versions(&a.version, &b.version))
}

fn visible_current_version(state: &MockState, org: &str, name: &str) -> Option<String> {
    let version = state.current.get(&(org.to_string(), name.to_string()))?;
    let metadata = state
        .metadata
        .get(&(org.to_string(), name.to_string(), version.clone()))?;
    metadata.yanked_at.is_none().then(|| version.clone())
}

/// Shared filter + row-building logic behind the mock `search` and
/// `list_remote` catalog calls. `query` is the search text; `None` lists
/// without text matching.
fn mock_catalog_rows(
    state: &MockState,
    query: Option<&str>,
    filters: &SearchFilters,
) -> Vec<RemoteSkill> {
    let q = query.map(str::to_ascii_lowercase);
    let mut out: Vec<RemoteSkill> = state
        .latest
        .keys()
        .filter_map(|(org, name)| {
            let meta = visible_latest_metadata(state, org, name)?;
            if filters
                .org
                .as_deref()
                .is_some_and(|filter| filter != meta.org)
            {
                return None;
            }
            if filters
                .team
                .as_deref()
                .is_some_and(|filter| meta.team.as_deref() != Some(filter))
            {
                return None;
            }
            if filters
                .visibility
                .is_some_and(|filter| filter != meta.visibility)
            {
                return None;
            }
            if let Some(owner) = filters.owner.as_deref()
                && meta.owner_email.as_deref() != Some(owner)
            {
                return None;
            }
            if !filters.platforms.is_empty()
                && !filters
                    .platforms
                    .iter()
                    .any(|filter| meta.platform_tags.iter().any(|tag| tag == filter))
            {
                return None;
            }
            if let Some(q) = &q
                && !q.is_empty()
            {
                let hay =
                    format!("{} {} {}", meta.org, meta.name, meta.description).to_ascii_lowercase();
                if !hay.contains(q) {
                    return None;
                }
            }
            let current_version = visible_current_version(state, org, name);
            Some(RemoteSkill {
                org: meta.org.clone(),
                name: meta.name.clone(),
                owner_email: meta.owner_email.clone(),
                latest_version: meta.version.clone(),
                current_version,
                description: meta.description.clone(),
                visibility: meta.visibility,
                team: meta.team.clone(),
                platform_tags: meta.platform_tags.clone(),
                updated_at: meta.updated_at.clone(),
                install_count: meta.install_count,
                last_installed_at: meta.last_installed_at.clone(),
            })
        })
        .collect();
    sort_catalog_rows(&mut out, filters.sort);
    if let Some(limit) = filters.limit {
        out.truncate(limit);
    }
    out
}

fn sort_catalog_rows(rows: &mut [RemoteSkill], sort: Option<CatalogSort>) {
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
        CatalogSort::Installs => rows.sort_by(|a, b| {
            b.install_count
                .cmp(&a.install_count)
                .then_with(|| a.org.cmp(&b.org))
                .then_with(|| a.name.cmp(&b.name))
        }),
    }
}

fn compare_registry_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let numeric_a = a.parse::<i64>().unwrap_or(0);
    let numeric_b = b.parse::<i64>().unwrap_or(0);
    numeric_a.cmp(&numeric_b).then_with(|| a.cmp(b))
}

/// Validate the shape of a registry URL. The rules are deliberately narrow:
///
/// - non-empty after trimming nothing (no leading/trailing whitespace);
/// - scheme must be `http://` or `https://` (case-insensitive);
/// - `http://` is only accepted for loopback hosts (`localhost`,
///   `127.0.0.0/8`, `::1`) so bearer tokens can never be sent in plaintext
///   to a real hosted registry;
/// - must include a non-empty host;
/// - must not include userinfo, query, or fragment components.
pub fn validate_registry_url(s: &str) -> Result<()> {
    RegistryUrl::parse(s)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn sample_metadata(org: &str, name: &str, version: &str) -> SkillMetadata {
        SkillMetadata {
            name: name.to_string(),
            description: format!("Use when working on {name}"),
            org: org.to_string(),
            owner_email: None,
            team: None,
            visibility: Visibility::Org,
            version: version.to_string(),
            hash: PackageHash::sha256_of(format!("{org}/{name}@{version}").as_bytes()),
            platform_tags: vec![],
            created_at: None,
            updated_at: None,
            install_count: None,
            last_installed_at: None,
            status: None,
            current: None,
            yanked_at: None,
            yank_reason: None,
            deprecated_at: None,
            deprecation_reason: None,
            audit_event_id: None,
        }
    }

    fn short_timeout_client(url: &str) -> HttpRegistryClient {
        HttpRegistryClient::with_timeouts(
            RegistryConnection::new(url, None),
            Duration::from_millis(50),
            Duration::from_millis(50),
        )
    }

    fn raw_http_server<F>(handler: F) -> (String, std::thread::JoinHandle<()>)
    where
        F: FnOnce(TcpStream) + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handler(stream);
        });
        (url, handle)
    }

    fn read_request_headers(stream: &mut TcpStream) {
        let mut request = Vec::new();
        let mut buf = [0_u8; 1024];
        loop {
            let n = stream.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            request.extend_from_slice(&buf[..n]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
    }

    #[test]
    fn registry_version_number_validation_accepts_positive_integer_strings() {
        assert_eq!(require_registry_version_number("1").unwrap(), "1");
        assert_eq!(
            require_registry_version_number("9223372036854775807").unwrap(),
            "9223372036854775807"
        );
    }

    #[test]
    fn registry_version_number_validation_rejects_non_server_versions() {
        for version in [
            "",
            "0",
            "-1",
            "1.2.3",
            "v1",
            "abc123",
            "9223372036854775808",
        ] {
            let err = require_registry_version_number(version).unwrap_err();
            assert!(
                err.to_string().contains("must be a positive integer"),
                "version: {version}, err: {err}"
            );
        }
    }

    #[test]
    fn stack_not_found_hint_uses_stack_guidance_without_skill_hint() {
        let url =
            Url::parse("https://registry.example.com/v1/orgs/acme/stacks/private/resolve").unwrap();

        let hint = status_hint(
            reqwest::StatusCode::NOT_FOUND,
            Some("stack_not_found"),
            &url,
        )
        .unwrap();

        assert!(
            hint.contains("agentstack stack list --org acme"),
            "hint: {hint}"
        );
        assert!(hint.contains("stack access"), "hint: {hint}");
        assert!(!hint.contains("agentstack skill search"), "hint: {hint}");
        assert!(
            !hint.contains("agentstack skill version list"),
            "hint: {hint}"
        );
    }

    #[test]
    fn skill_not_found_hints_keep_skill_guidance() {
        let url = Url::parse("https://registry.example.com/v1/orgs/acme/skills/missing").unwrap();

        for code in ["skill_not_found", "version_not_found"] {
            let hint = status_hint(reqwest::StatusCode::NOT_FOUND, Some(code), &url).unwrap();

            assert!(hint.contains("agentstack skill search"), "hint: {hint}");
            assert!(
                hint.contains("agentstack skill version list"),
                "hint: {hint}"
            );
            assert!(!hint.contains("agentstack stack list"), "hint: {hint}");
        }
    }

    #[test]
    fn audit_event_not_found_hint_uses_audit_guidance() {
        let url =
            Url::parse("https://registry.example.com/v1/orgs/acme/audit/aud_missing").unwrap();

        let hint = status_hint(
            reqwest::StatusCode::NOT_FOUND,
            Some("audit_event_not_found"),
            &url,
        )
        .unwrap();
        let next = status_next_command(
            reqwest::StatusCode::NOT_FOUND,
            Some("audit_event_not_found"),
            &url,
        )
        .unwrap();

        assert!(
            hint.contains("agentstack audit list --org acme"),
            "hint: {hint}"
        );
        assert!(!hint.contains("agentstack skill search"), "hint: {hint}");
        assert_eq!(next, "agentstack audit list --org acme");
    }

    #[test]
    fn versions_collection_url_resource_uses_skill_ref() {
        let url = Url::parse("https://registry.example.com/v1/orgs/acme/skills/missing/versions")
            .unwrap();

        assert_eq!(resource_from_url(&url).as_deref(), Some("acme/missing"));
    }

    #[test]
    fn no_current_version_hint_names_approval_command() {
        let url =
            Url::parse("https://registry.example.com/v1/orgs/acme/skills/sql-review").unwrap();

        let hint = status_hint(
            reqwest::StatusCode::CONFLICT,
            Some("no_current_version"),
            &url,
        )
        .unwrap();

        assert!(
            hint.contains("no approved current version yet; ask an admin to run:"),
            "hint: {hint}"
        );
        assert!(
            hint.contains("agentstack skill version approve acme/sql-review@<VERSION>"),
            "hint: {hint}"
        );
    }

    #[test]
    fn validates_basic_urls() {
        validate_registry_url("https://registry.example.com").unwrap();
        validate_registry_url("http://localhost:8080").unwrap();
        validate_registry_url("HTTPS://EXAMPLE.com").unwrap();
        validate_registry_url("https://example.com/v1").unwrap();
        validate_registry_url("https://example.com/").unwrap();
        validate_registry_url("https://example.com/v1/").unwrap();
    }

    #[test]
    fn rejects_bad_urls() {
        assert!(validate_registry_url("").is_err());
        assert!(validate_registry_url(" https://x").is_err());
        assert!(validate_registry_url("https://x ").is_err());
        assert!(validate_registry_url("ftp://example.com").is_err());
        assert!(validate_registry_url("example.com").is_err());
        assert!(validate_registry_url("https://").is_err());
        assert!(validate_registry_url("https:///path").is_err());
        assert!(validate_registry_url("https:// space.com").is_err());
        assert!(validate_registry_url("https://token@example.com").is_err());
        assert!(validate_registry_url("https://example.com?token=secret").is_err());
        assert!(validate_registry_url("https://example.com#token").is_err());
    }

    #[test]
    fn allows_http_only_for_loopback_hosts() {
        validate_registry_url("http://localhost").unwrap();
        validate_registry_url("http://LOCALHOST:8080").unwrap();
        validate_registry_url("http://127.0.0.1").unwrap();
        validate_registry_url("http://127.0.0.1:8080/v1").unwrap();
        validate_registry_url("http://127.5.6.7:9000").unwrap();
        validate_registry_url("http://[::1]:8080").unwrap();
    }

    #[test]
    fn rejects_plain_http_for_non_loopback_hosts() {
        let err = RegistryUrl::parse("http://registry.agentstack.gg").unwrap_err();
        assert!(matches!(err, RegistryUrlError::InsecureScheme));
        let msg = err.to_string();
        assert!(msg.contains("https://"), "msg: {msg}");
        assert!(msg.contains("loopback"), "msg: {msg}");

        assert!(matches!(
            RegistryUrl::parse("http://example.com").unwrap_err(),
            RegistryUrlError::InsecureScheme
        ));
        assert!(matches!(
            RegistryUrl::parse("http://10.0.0.5:8080").unwrap_err(),
            RegistryUrlError::InsecureScheme
        ));
        assert!(matches!(
            RegistryUrl::parse("http://[2001:db8::1]:8080").unwrap_err(),
            RegistryUrlError::InsecureScheme
        ));
    }

    #[test]
    fn http_client_builds_v1_urls_from_configured_base() {
        let client =
            HttpRegistryClient::new(RegistryConnection::new("http://127.0.0.1:8080/", None));
        assert_eq!(
            client.api_url("/ping").unwrap(),
            "http://127.0.0.1:8080/v1/ping"
        );

        let client =
            HttpRegistryClient::new(RegistryConnection::new("http://127.0.0.1:8080/v1", None));
        assert_eq!(
            client.api_url("/whoami").unwrap(),
            "http://127.0.0.1:8080/v1/whoami"
        );

        let client =
            HttpRegistryClient::new(RegistryConnection::new("http://127.0.0.1:8080/v1/", None));
        assert_eq!(
            client.api_url("/ping").unwrap(),
            "http://127.0.0.1:8080/v1/ping"
        );
        assert_eq!(
            client.api_url("/search?q=sql+review").unwrap(),
            "http://127.0.0.1:8080/v1/search?q=sql+review"
        );
        assert_eq!(
            client
                .api_url("/orgs/acme/skills/1.2.3%2Bbuild%235")
                .unwrap(),
            "http://127.0.0.1:8080/v1/orgs/acme/skills/1.2.3%2Bbuild%235"
        );
    }

    #[test]
    fn http_client_encodes_paths_and_queries() {
        assert_eq!(encode_path_segment("1.2.3+build#5"), "1.2.3%2Bbuild%235");
        assert_eq!(encode_query_component("sql review"), "sql+review");
    }

    #[test]
    fn http_client_requires_token_except_ping_helpers() {
        let client =
            HttpRegistryClient::new(RegistryConnection::new("http://127.0.0.1:8080", None));
        let request = client.http.get(client.api_url("/whoami").unwrap());
        let err = client.authenticated(request).unwrap_err().to_string();
        assert!(err.contains("not logged in"));
    }

    #[test]
    fn decode_bytes_rejects_declared_content_length_over_limit_before_body_read() {
        let (release_tx, release_rx) = mpsc::channel();
        let (url, handle) = raw_http_server(move |mut stream| {
            read_request_headers(&mut stream);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 6\r\nconnection: close\r\n\r\n")
                .unwrap();
            release_rx.recv().ok();
        });
        let client = short_timeout_client(&url);
        let response = client.http.get(format!("{url}/archive")).send().unwrap();

        let start = Instant::now();
        let err = client.decode_bytes_with_limit(response, 5).unwrap_err();
        let elapsed = start.elapsed();
        release_tx.send(()).ok();
        handle.join().unwrap();

        let msg = err.to_string();
        assert!(
            elapsed < Duration::from_millis(100),
            "content-length rejection should not wait for body read: {elapsed:?}"
        );
        assert!(msg.contains("download limit"), "msg: {msg}");
        assert!(msg.contains("5"), "msg: {msg}");
    }

    #[test]
    fn decode_bytes_rejects_streaming_body_over_limit_without_content_length() {
        let (release_tx, release_rx) = mpsc::channel();
        let (url, handle) = raw_http_server(move |mut stream| {
            read_request_headers(&mut stream);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nconnection: keep-alive\r\n\r\n")
                .unwrap();
            stream.write_all(&[b'x'; 6]).unwrap();
            release_rx.recv().ok();
        });
        let client = short_timeout_client(&url);
        let response = client.http.get(format!("{url}/archive")).send().unwrap();
        assert_eq!(response.content_length(), None);

        let err = client.decode_bytes_with_limit(response, 5).unwrap_err();
        release_tx.send(()).ok();
        handle.join().unwrap();

        let msg = err.to_string();
        assert!(msg.contains("download limit"), "msg: {msg}");
        assert!(msg.contains("5"), "msg: {msg}");
    }

    #[test]
    fn http_client_can_use_short_timeout_against_stalling_server() {
        let (release_tx, release_rx) = mpsc::channel();
        let (url, handle) = raw_http_server(move |mut stream| {
            read_request_headers(&mut stream);
            release_rx.recv().ok();
        });
        let client = short_timeout_client(&url);

        let start = Instant::now();
        let err = client.ping().unwrap_err();
        let elapsed = start.elapsed();
        release_tx.send(()).ok();
        handle.join().unwrap();

        assert!(
            elapsed < Duration::from_secs(1),
            "short test timeout should fail quickly, elapsed: {elapsed:?}"
        );
        assert!(
            err.to_string().contains("registry request failed"),
            "msg: {err}"
        );
    }

    #[test]
    fn http_client_does_not_follow_redirects() {
        // A redirect target that would satisfy `ping` if the client followed
        // the redirect; the API contract defines no redirecting endpoints,
        // and following one could replay the bearer token to another origin.
        let (target_url, target_handle) = raw_http_server(move |mut stream| {
            read_request_headers(&mut stream);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{\"status\":\"ok\",\"server_version\":\"test\"}",
            );
        });
        let location = format!("{target_url}/v1/ping");
        let (url, handle) = raw_http_server(move |mut stream| {
            read_request_headers(&mut stream);
            let response = format!(
                "HTTP/1.1 302 Found\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        let client = short_timeout_client(&url);

        let err = client.ping().unwrap_err();
        handle.join().unwrap();
        // Unblock the redirect target's accept() so its thread can exit.
        TcpStream::connect(target_url.strip_prefix("http://").unwrap()).ok();
        target_handle.join().unwrap();

        let msg = err.to_string();
        assert!(msg.contains("302"), "msg: {msg}");
    }

    #[test]
    fn oauth_error_codes_remain_stable() {
        for code in [
            "oauth_denied",
            "oauth_expired",
            "oauth_invalid_grant",
            "invite_required",
        ] {
            assert_eq!(stable_registry_error_code(code), code);
        }
    }

    #[test]
    fn sanitizer_redacts_oauth_secret_material() {
        let text = "code=abc123 code_verifier=verifiersecret code_challenge=challengesecret state=statesecret access_token=tokensecret";
        let redacted = sanitize_registry_error_text(text, None);

        for secret in [
            "abc123",
            "verifiersecret",
            "challengesecret",
            "statesecret",
            "tokensecret",
        ] {
            assert!(!redacted.contains(secret), "redacted: {redacted}");
        }
        assert!(redacted.contains("[REDACTED]"), "redacted: {redacted}");
    }

    #[test]
    fn mock_round_trips_push_pull() {
        let mock = MockRegistryClient::with_user("octocat");
        mock.set_server_version("1.0.0");

        let ping = mock.ping().unwrap();
        assert_eq!(ping.status, "ok");
        assert_eq!(ping.server_version, "1.0.0");
        assert_eq!(mock.whoami().unwrap().user, "octocat");

        let meta = sample_metadata("acme", "alpha", "0.1.0");
        let resp = mock
            .push(PushRequest {
                metadata: meta.clone(),
                archive: b"alpha-bytes",
            })
            .unwrap();
        assert_eq!(resp.metadata.org, "acme");
        assert_eq!(resp.metadata.name, "alpha");
        assert_eq!(resp.skill_ref, "acme/alpha@1");
        assert_eq!(resp.version, "1");
        assert_eq!(resp.sha256, meta.hash.hex);
        assert_eq!(resp.visibility, Visibility::Org);
        assert!(resp.metadata.created_at.is_some());
        assert!(resp.url.as_deref().unwrap().contains("alpha"));

        // Pull pinned version.
        let r: SkillRef = "acme/alpha@1".parse().unwrap();
        let pulled = mock.pull(&r).unwrap();
        assert_eq!(pulled.archive, b"alpha-bytes");
        assert_eq!(pulled.metadata.hash, meta.hash);

        let approved = mock.approve(&r, "1").unwrap();
        assert_eq!(approved.status, Some(VersionStatus::Approved));
        assert_eq!(approved.current, Some(true));

        // Pull current approved.
        let r: SkillRef = "acme/alpha".parse().unwrap();
        let pulled = mock.pull(&r).unwrap();
        assert_eq!(pulled.archive, b"alpha-bytes");
        assert_eq!(pulled.metadata.version, "1");
    }

    #[test]
    fn mock_search_filters_by_query() {
        let mock = MockRegistryClient::new();
        mock.push(PushRequest {
            metadata: SkillMetadata {
                description: "Use when reviewing pull requests".to_string(),
                ..sample_metadata("acme", "code-review", "0.1.0")
            },
            archive: b"a",
        })
        .unwrap();
        mock.push(PushRequest {
            metadata: SkillMetadata {
                description: "Use to format markdown".to_string(),
                ..sample_metadata("acme", "format-md", "0.1.0")
            },
            archive: b"b",
        })
        .unwrap();

        let hits = mock.search("review").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "code-review");

        let all = mock.search("").unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn mock_list_remote_filters_by_org() {
        let mock = MockRegistryClient::new();
        mock.push(PushRequest {
            metadata: sample_metadata("acme", "x", "0.1.0"),
            archive: b"a",
        })
        .unwrap();
        mock.push(PushRequest {
            metadata: sample_metadata("widgets", "y", "0.1.0"),
            archive: b"b",
        })
        .unwrap();

        let acme = mock.list_remote(Some("acme")).unwrap();
        assert_eq!(acme.len(), 1);
        assert_eq!(acme[0].org, "acme");

        let all = mock.list_remote(None).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn mock_list_versions_returns_all_known_versions() {
        let mock = MockRegistryClient::new();
        for v in ["0.1.0", "0.2.0", "0.3.0"] {
            mock.push(PushRequest {
                metadata: sample_metadata("acme", "x", v),
                archive: format!("bytes-{v}").as_bytes(),
            })
            .unwrap();
        }
        let r: SkillRef = "acme/x".parse().unwrap();
        let versions = mock.list_versions(&r).unwrap();
        assert_eq!(versions.len(), 3);
        let v_names: Vec<_> = versions.iter().map(|v| v.version.as_str()).collect();
        assert_eq!(v_names, vec!["3", "2", "1"]);
    }

    #[test]
    fn mock_surfaces_platform_tags_in_registry_views() {
        let mock = MockRegistryClient::new();
        let platform_tags = vec!["claude-code".to_string(), "codex".to_string()];
        mock.push(PushRequest {
            metadata: SkillMetadata {
                platform_tags: platform_tags.clone(),
                ..sample_metadata("acme", "alpha", "0.1.0")
            },
            archive: b"a",
        })
        .unwrap();

        let search = mock.search("alpha").unwrap();
        assert_eq!(search[0].platform_tags, platform_tags);

        let list = mock.list_remote(Some("acme")).unwrap();
        assert_eq!(list[0].platform_tags, platform_tags);

        let r: SkillRef = "acme/alpha".parse().unwrap();
        let versions = mock.list_versions(&r).unwrap();
        assert_eq!(versions[0].platform_tags, platform_tags);
    }

    #[test]
    fn mock_surfaces_install_metrics_and_sorts_by_installs() {
        let mock = MockRegistryClient::new();
        mock.seed(
            SkillMetadata {
                install_count: Some(3),
                last_installed_at: Some("2026-06-01T00:00:00Z".to_string()),
                current: Some(true),
                ..sample_metadata("acme", "alpha", "1")
            },
            b"a".to_vec(),
        );
        mock.seed(
            SkillMetadata {
                install_count: Some(9),
                last_installed_at: Some("2026-06-02T00:00:00Z".to_string()),
                current: Some(true),
                ..sample_metadata("acme", "beta", "1")
            },
            b"b".to_vec(),
        );
        mock.seed(
            SkillMetadata {
                current: Some(true),
                ..sample_metadata("acme", "gamma", "1")
            },
            b"c".to_vec(),
        );

        let filters = SearchFilters {
            sort: Some(CatalogSort::Installs),
            ..SearchFilters::default()
        };
        let listed = mock.list_remote_with_filters(&filters).unwrap();
        let names: Vec<_> = listed.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, vec!["beta", "alpha", "gamma"]);
        assert_eq!(listed[0].install_count, Some(9));
        assert_eq!(
            listed[0].last_installed_at.as_deref(),
            Some("2026-06-02T00:00:00Z")
        );
        assert_eq!(listed[2].install_count, None);

        let searched = mock.search_with_filters("", &filters).unwrap();
        let names: Vec<_> = searched.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, vec!["beta", "alpha", "gamma"]);
    }

    #[test]
    fn mock_list_versions_unknown_skill_errors() {
        let mock = MockRegistryClient::new();
        let r: SkillRef = "acme/missing".parse().unwrap();
        assert!(mock.list_versions(&r).is_err());
    }

    #[test]
    fn mock_can_fail_ping_on_demand() {
        let mock = MockRegistryClient::new();
        mock.fail_next_ping("simulated network down");
        let err = mock.ping().unwrap_err();
        assert!(err.to_string().contains("simulated network down"));
        // Subsequent calls succeed because we only injected one failure.
        mock.set_server_version("0.1.0");
        assert!(mock.ping().is_ok());
    }

    #[test]
    fn visibility_round_trips_via_string() {
        for v in [Visibility::Private, Visibility::Org, Visibility::Team] {
            let parsed: Visibility = v.as_str().parse().unwrap();
            assert_eq!(parsed, v);
        }
        assert!("public".parse::<Visibility>().is_err());
        let err = "public".parse::<Visibility>().unwrap_err().to_string();
        assert!(
            err.contains("expected one of: private, org, team"),
            "msg: {err}"
        );
    }
}
