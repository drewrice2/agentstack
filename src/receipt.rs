//! Install receipts — provenance written next to every installed skill.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::package::PackageHash;
use crate::registry::Visibility;
use crate::skill::check_slug;

pub const RECEIPT_FILE: &str = ".agentstack-install.json";
pub const STACK_RECEIPT_FILE: &str = ".agentstack.json";
pub const RECEIPT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReceiptSourceType {
    Local,
    Registry,
}

impl ReceiptSourceType {
    pub const fn as_str(self) -> &'static str {
        match self {
            ReceiptSourceType::Local => "local",
            ReceiptSourceType::Registry => "registry",
        }
    }

    /// Machine-readable kind of the recorded hash, used in JSON output.
    pub const fn hash_kind(self) -> &'static str {
        match self {
            ReceiptSourceType::Registry => "package",
            ReceiptSourceType::Local => "install_tree",
        }
    }

    /// Human-readable label for the recorded hash in `show` output.
    pub const fn hash_label(self) -> &'static str {
        match self {
            ReceiptSourceType::Registry => "package hash",
            ReceiptSourceType::Local => "install tree hash",
        }
    }

    /// Hash kind column value in `install list` tables (hyphenated, unlike
    /// the underscored JSON [`Self::hash_kind`]).
    pub const fn hash_kind_column(self) -> &'static str {
        match self {
            ReceiptSourceType::Registry => "package",
            ReceiptSourceType::Local => "install-tree",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallReceipt {
    pub schema_version: u32,
    pub skill_name: String,
    pub source_type: ReceiptSourceType,
    pub source_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    pub target: String,
    pub installed_path: PathBuf,
    pub installed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_via: Option<InstallVia>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub installed_via_stacks: Vec<InstallVia>,
}

#[derive(Debug, Clone)]
pub struct InstallReceiptRequest {
    pub source_type: ReceiptSourceType,
    pub source_ref: String,
    pub registry_url: Option<String>,
    pub org: Option<String>,
    pub version: Option<String>,
    pub hash: Option<PackageHash>,
    pub content_hash: Option<PackageHash>,
    pub target: String,
    pub installed_by: Option<String>,
    pub installed_via: Option<InstallVia>,
    pub installed_via_stacks: Vec<InstallVia>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallVia {
    pub kind: String,
    pub org: String,
    pub stack: String,
    pub manifest_hash: String,
}

impl InstallVia {
    pub fn stack(org: String, stack: String, manifest_hash: &PackageHash) -> Self {
        Self {
            kind: "stack".to_string(),
            org,
            stack,
            manifest_hash: format_hash(manifest_hash),
        }
    }
}

pub fn stack_referrers(receipt: &InstallReceipt) -> Vec<InstallVia> {
    let mut refs = Vec::new();
    if let Some(via) = &receipt.installed_via
        && via.kind == "stack"
    {
        push_unique_stack_ref(&mut refs, via.clone());
    }
    for via in &receipt.installed_via_stacks {
        if via.kind == "stack" {
            push_unique_stack_ref(&mut refs, via.clone());
        }
    }
    refs
}

pub fn push_unique_stack_ref(refs: &mut Vec<InstallVia>, via: InstallVia) {
    if refs.iter().any(|existing| {
        existing.kind == via.kind && existing.org == via.org && existing.stack == via.stack
    }) {
        return;
    }
    refs.push(via);
}

pub fn remove_stack_referrer(refs: &mut Vec<InstallVia>, org: &str, stack: &str) {
    refs.retain(|via| !(via.kind == "stack" && via.org == org && via.stack == stack));
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackInstallReceipt {
    pub schema_version: u32,
    pub kind: String,
    pub org: String,
    pub stack: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_url: Option<String>,
    pub visibility: Visibility,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    pub resolved_at: String,
    pub manifest_hash: PackageHash,
    pub target: String,
    pub installed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_by: Option<String>,
    pub items: Vec<StackInstallReceiptItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackInstallReceiptItem {
    pub skill: String,
    pub version_id: String,
    pub version: String,
    pub archive_hash: PackageHash,
    pub install_path: PathBuf,
    pub installed_receipt_path: PathBuf,
}

/// A user-supplied `[org/]stack` argument naming a managed stack install.
#[derive(Debug, Clone)]
pub struct StackLookup {
    pub org: Option<String>,
    pub stack: String,
}

impl StackLookup {
    pub fn parse(raw: &str) -> Result<Self> {
        if let Some((org, stack)) = raw.split_once('/') {
            check_slug(org).map_err(|reason| anyhow::anyhow!("invalid org `{org}`: {reason}"))?;
            check_slug(stack)
                .map_err(|reason| anyhow::anyhow!("invalid stack `{stack}`: {reason}"))?;
            return Ok(StackLookup {
                org: Some(org.to_string()),
                stack: stack.to_string(),
            });
        }

        check_slug(raw).map_err(|reason| anyhow::anyhow!("invalid stack `{raw}`: {reason}"))?;
        Ok(StackLookup {
            org: None,
            stack: raw.to_string(),
        })
    }

    /// `org/stack`, or the bare stack slug when no org was given.
    pub fn label(&self) -> String {
        self.org
            .as_ref()
            .map(|org| format!("{org}/{}", self.stack))
            .unwrap_or_else(|| self.stack.clone())
    }

    /// Like [`Self::label`], but renders a `<org>/` placeholder when no org
    /// was given, for messages that quote a copy-pasteable `org/stack` ref.
    pub fn label_with_org_placeholder(&self) -> String {
        self.org
            .as_ref()
            .map(|org| format!("{org}/{}", self.stack))
            .unwrap_or_else(|| format!("<org>/{}", self.stack))
    }
}

impl InstallReceipt {
    pub fn from_request(
        skill_name: String,
        installed_path: PathBuf,
        request: InstallReceiptRequest,
    ) -> Result<Self> {
        let mut installed_via_stacks = request.installed_via_stacks;
        if let Some(via) = &request.installed_via
            && via.kind == "stack"
        {
            push_unique_stack_ref(&mut installed_via_stacks, via.clone());
        }
        Ok(Self {
            schema_version: RECEIPT_SCHEMA_VERSION,
            skill_name,
            source_type: request.source_type,
            source_ref: request.source_ref,
            registry_url: request.registry_url,
            org: request.org,
            version: request.version,
            hash: request.hash.as_ref().map(format_hash),
            content_hash: request.content_hash.as_ref().map(format_hash),
            target: request.target,
            installed_path,
            installed_at: installed_timestamp()?,
            installed_by: request.installed_by,
            installed_via: request.installed_via,
            installed_via_stacks,
        })
    }
}

pub fn format_hash(hash: &PackageHash) -> String {
    format!("{}:{}", hash.algorithm, hash.hex)
}

pub fn installed_timestamp() -> Result<String> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("failed to format install timestamp")
}

pub fn local_installed_by() -> Option<String> {
    std::env::var("USER")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("USERNAME")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
}

pub fn receipt_path(installed_path: &Path) -> PathBuf {
    installed_path.join(RECEIPT_FILE)
}

pub fn validate_stack_receipt_item_paths(
    target_root: &Path,
    item: &StackInstallReceiptItem,
) -> Result<()> {
    crate::skill::check_slug(&item.skill).map_err(|reason| {
        anyhow::anyhow!(
            "refusing stack receipt item `{}` because skill name is invalid: {reason}",
            item.skill
        )
    })?;
    let target_root = lexical_absolute_path(target_root)?;
    let install_path = lexical_absolute_path(&item.install_path)?;
    if install_path == target_root || !install_path.starts_with(&target_root) {
        bail!(
            "refusing stack receipt item `{}` because install path `{}` is outside target root `{}`",
            item.skill,
            item.install_path.display(),
            target_root.display()
        );
    }
    let expected_install_path = lexical_absolute_path(&target_root.join(&item.skill))?;
    if install_path != expected_install_path {
        bail!(
            "refusing stack receipt item `{}` because install path `{}` does not match expected path `{}`",
            item.skill,
            item.install_path.display(),
            expected_install_path.display()
        );
    }

    let expected_receipt_path = receipt_path(&item.install_path);
    if item.installed_receipt_path != expected_receipt_path {
        bail!(
            "refusing stack receipt item `{}` because installed receipt path `{}` does not match expected path `{}`",
            item.skill,
            item.installed_receipt_path.display(),
            expected_receipt_path.display()
        );
    }

    Ok(())
}

fn lexical_absolute_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("failed to resolve current directory")?
            .join(path)
    };

    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

pub fn write_receipt_to_dir(installed_dir: &Path, receipt: &InstallReceipt) -> Result<PathBuf> {
    let path = receipt_path(installed_dir);
    let json =
        serde_json::to_string_pretty(receipt).context("failed to serialize install receipt")?;
    crate::fs_atomic::write_string(&path, &json)
        .with_context(|| format!("failed to write `{}`", path.display()))?;
    Ok(path)
}

pub fn stack_receipt_path(target_root: &Path, org: &str, stack: &str) -> PathBuf {
    target_root
        .join(".agentstack-stacks")
        .join(org)
        .join(stack)
        .join(STACK_RECEIPT_FILE)
}

pub fn ensure_stack_receipt_dir_not_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "`{}` is a symlink; refusing to use it for stack receipts",
            path.display()
        ),
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to stat `{}`", path.display())),
    }
}

pub fn write_stack_receipt(target_root: &Path, receipt: &StackInstallReceipt) -> Result<PathBuf> {
    let path = stack_receipt_path(target_root, &receipt.org, &receipt.stack);
    let stacks_root = target_root.join(".agentstack-stacks");
    let org_dir = stacks_root.join(&receipt.org);
    let stack_dir = org_dir.join(&receipt.stack);
    ensure_stack_receipt_dir_not_symlink(&stacks_root)?;
    ensure_stack_receipt_dir_not_symlink(&org_dir)?;
    ensure_stack_receipt_dir_not_symlink(&stack_dir)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create `{}`", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(receipt)
        .context("failed to serialize stack install receipt")?;
    crate::fs_atomic::write_string(&path, &json)
        .with_context(|| format!("failed to write `{}`", path.display()))?;
    Ok(path)
}

pub fn read_receipt_from_dir(installed_dir: &Path) -> Result<InstallReceipt> {
    read_receipt_file(&receipt_path(installed_dir))
}

pub fn read_receipt_file(path: &Path) -> Result<InstallReceipt> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read `{}`", path.display()))?;
    let receipt: InstallReceipt = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse `{}`", path.display()))?;
    if receipt.schema_version != RECEIPT_SCHEMA_VERSION {
        bail!(
            "install receipt at `{}` is schema_version {}; this CLI understands version {}; upgrade agentstack",
            path.display(),
            receipt.schema_version,
            RECEIPT_SCHEMA_VERSION
        );
    }
    Ok(receipt)
}

pub fn read_stack_receipt_file(path: &Path) -> Result<StackInstallReceipt> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read `{}`", path.display()))?;
    let receipt: StackInstallReceipt = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse `{}`", path.display()))?;
    if receipt.schema_version != RECEIPT_SCHEMA_VERSION {
        bail!(
            "stack install receipt at `{}` is schema_version {}; this CLI understands version {}; upgrade agentstack",
            path.display(),
            receipt.schema_version,
            RECEIPT_SCHEMA_VERSION
        );
    }
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_fs::fixture::PathChild;

    #[test]
    fn read_receipt_file_rejects_unknown_schema_version() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let path = tmp.child(RECEIPT_FILE);
        std::fs::write(
            path.path(),
            r#"{
  "schema_version": 99,
  "skill_name": "demo",
  "source_type": "local",
  "source_ref": "/tmp/demo",
  "target": "claude-code",
  "installed_path": "/tmp/target/demo",
  "installed_at": "2026-05-09T00:00:00Z"
}"#,
        )
        .unwrap();

        let err = read_receipt_file(path.path()).unwrap_err();
        assert!(err.to_string().contains("schema_version"));
    }

    #[test]
    fn write_receipt_to_dir_preserves_pretty_json_format() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let receipt = InstallReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            skill_name: "demo".to_string(),
            source_type: ReceiptSourceType::Local,
            source_ref: "/tmp/demo".to_string(),
            registry_url: None,
            org: None,
            version: None,
            hash: Some("sha256:abc123".to_string()),
            content_hash: None,
            target: "local".to_string(),
            installed_path: tmp.path().join("demo"),
            installed_at: "2026-05-09T00:00:00Z".to_string(),
            installed_by: Some("dex".to_string()),
            installed_via: None,
            installed_via_stacks: Vec::new(),
        };

        let path = write_receipt_to_dir(tmp.path(), &receipt).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, serde_json::to_string_pretty(&receipt).unwrap());
        assert_eq!(read_receipt_from_dir(tmp.path()).unwrap(), receipt);
    }

    #[test]
    fn stack_receipt_item_path_validation_requires_expected_receipt_path() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let target_root = tmp.path().join("target");
        let install_path = target_root.join("alpha");
        let item = StackInstallReceiptItem {
            skill: "alpha".to_string(),
            version_id: "1".to_string(),
            version: "1".to_string(),
            archive_hash: PackageHash {
                algorithm: "sha256".to_string(),
                hex: "abc".to_string(),
            },
            install_path,
            installed_receipt_path: target_root.join("other/.agentstack-install.json"),
        };

        let err = validate_stack_receipt_item_paths(&target_root, &item).unwrap_err();
        assert!(err.to_string().contains("does not match expected path"));
    }

    #[test]
    fn stack_receipt_item_path_validation_requires_skill_path() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let target_root = tmp.path().join("target");
        let install_path = target_root.join("other");
        let item = StackInstallReceiptItem {
            skill: "alpha".to_string(),
            version_id: "1".to_string(),
            version: "1".to_string(),
            archive_hash: PackageHash {
                algorithm: "sha256".to_string(),
                hex: "abc".to_string(),
            },
            installed_receipt_path: receipt_path(&install_path),
            install_path,
        };

        let err = validate_stack_receipt_item_paths(&target_root, &item).unwrap_err();
        assert!(err.to_string().contains("does not match expected path"));
    }

    #[test]
    fn stack_receipt_item_path_validation_rejects_path_shaped_skill() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let target_root = tmp.path().join("target");
        let install_path = target_root.join(".agentstack-stacks/acme/foo");
        let item = StackInstallReceiptItem {
            skill: ".agentstack-stacks/acme/foo".to_string(),
            version_id: "1".to_string(),
            version: "1".to_string(),
            archive_hash: PackageHash {
                algorithm: "sha256".to_string(),
                hex: "abc".to_string(),
            },
            installed_receipt_path: receipt_path(&install_path),
            install_path,
        };

        let err = validate_stack_receipt_item_paths(&target_root, &item).unwrap_err();
        assert!(err.to_string().contains("skill name is invalid"));
    }
}
