//! Workflow tests for registry push, export, search, list, and
//! `versions` driven against an in-memory [`MockRegistryClient`].
//!
//! The CLI integration tests exercise the parsing + flag plumbing through
//! the binary; these tests exercise the workflow logic directly so we can
//! assert the request shape sent to the registry and inspect the unpacked
//! filesystem result.
//!
//! [`MockRegistryClient`]: agentstack::registry::MockRegistryClient

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::thread;

use agentstack::commands::{
    BatchUpdateRow, BatchUpdateRowStatus, CandidatesOptions, DiffOptions, ExportOptions,
    InstalledDiffOptions, PushOptions, RemoteInstallOptions, StackInstallOptions,
    StackUpdateOptions, UpdateAllOptions, UpdateOptions, YankAction, YankOptions,
    approve_with_client, candidates_with_client, collect_candidates, diff_installed_with_client,
    diff_with_client, install_remote_with_client, install_stack_with_client,
    list_remote_with_client, push_with_client, registry_export_with_client, search_with_client,
    update_all_with_client, update_stack_with_client, update_with_client, versions_with_client,
    yank_with_client,
};
use agentstack::error::CliError;
use agentstack::install::{InstallOptions, install_skill};
use agentstack::package::{PackageHash, build_skill_package};
use agentstack::receipt::{
    InstallReceiptRequest, ReceiptSourceType, format_hash, read_receipt_from_dir, receipt_path,
};
use agentstack::registry::{
    CatalogSort, MockRegistryClient, PullClientOptions, PullResponse, PushRequest, RegistryClient,
    SearchFilters, SkillMetadata, VersionPolicy, VersionStatus, Visibility,
};
use agentstack::skill::{LintConfig, lint_skill, validate_skill};
use agentstack::skill_ref::SkillRef;
use agentstack::targets::InstallTarget;

fn unique_dir(prefix: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "agentstack-registry-{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    p
}

fn make_skill(dir: &Path, name: &str, description: &str) {
    fs::create_dir_all(dir).unwrap();
    let body = format!(
        "---\nname: {name}\ndescription: {description}\n---\n\n# Purpose\n\nSee references/notes.md when present.\n\n# When to Use\n\n# Instructions\n\n# Output\n\n# Boundaries\n",
    );
    fs::write(dir.join("SKILL.md"), body).unwrap();
}

fn assert_valid_and_lints_clean(path: &Path) {
    let outcome = validate_skill(path);
    assert!(
        outcome.is_ok(),
        "expected valid skill at `{}`: {:?}",
        path.display(),
        outcome.errors
    );
    let warnings = lint_skill(
        path,
        outcome.parsed.as_ref().unwrap(),
        outcome.content.as_deref().unwrap(),
        &LintConfig::default(),
    );
    assert!(
        warnings.is_empty(),
        "expected lint-clean skill at `{}`: {warnings:?}",
        path.display()
    );
}

fn receipt(path: &Path) -> serde_json::Value {
    let text = fs::read_to_string(path.join(".agentstack-install.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn stack_receipt(target_root: &Path, org: &str, stack: &str) -> serde_json::Value {
    let text = fs::read_to_string(
        target_root
            .join(".agentstack-stacks")
            .join(org)
            .join(stack)
            .join(".agentstack.json"),
    )
    .unwrap();
    serde_json::from_str(&text).unwrap()
}

fn publish_approved_skill(mock: &MockRegistryClient, source: &Path, name: &str) {
    push_with_client(
        Some(mock),
        None,
        PushOptions {
            source,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();
    mock.approve(&format!("acme/{name}").parse().unwrap(), "1")
        .unwrap();
}

#[test]
fn diff_compares_local_skill_contents() {
    let scratch = unique_dir("diff-local");
    let left = scratch.join("left").join("alpha");
    let right = scratch.join("right").join("alpha");
    make_skill(&left, "alpha", "Use when working on alpha tasks");
    make_skill(&right, "alpha", "Use when working on alpha tasks");
    fs::create_dir_all(left.join("references")).unwrap();
    fs::write(left.join("references/removed.md"), "old note\n").unwrap();
    fs::create_dir_all(right.join("references")).unwrap();
    fs::write(right.join("references/added.md"), "new note\n").unwrap();
    fs::write(
        right.join("SKILL.md"),
        "---\nname: alpha\ndescription: Use when working on alpha tasks\n---\n\n# Purpose\n\nUpdated.\n\n# When to Use\n\n# Instructions\n\n# Output\n\n# Boundaries\n",
    )
    .unwrap();

    let mock = MockRegistryClient::new();
    let outcome = diff_with_client(
        &mock,
        None,
        DiffOptions {
            json: false,
            left: left.to_str().unwrap(),
            right: right.to_str().unwrap(),
            quiet: true,
            allow_yanked: false,
        },
    )
    .unwrap();

    assert_eq!(outcome.added, vec!["references/added.md"]);
    assert_eq!(outcome.removed, vec!["references/removed.md"]);
    assert_eq!(outcome.changed.len(), 1);
    assert_eq!(outcome.changed[0].path, "SKILL.md");
    assert_eq!(outcome.unchanged_count, 0);
    assert!(!outcome.is_empty);
}

#[test]
fn diff_compares_registry_versions_without_writing_outputs() {
    let scratch = unique_dir("diff-registry");
    let source = scratch.join("alpha");
    make_skill(&source, "alpha", "Use when working on alpha tasks");

    let mock = MockRegistryClient::with_user("publisher@example.com");
    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();
    mock.approve(&"acme/alpha".parse().unwrap(), "1").unwrap();

    fs::create_dir_all(source.join("references")).unwrap();
    fs::write(source.join("references/new.md"), "new registry note\n").unwrap();
    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();

    let outcome = diff_with_client(
        &mock,
        None,
        DiffOptions {
            json: false,
            left: "acme/alpha@1",
            right: "acme/alpha@2",
            quiet: true,
            allow_yanked: false,
        },
    )
    .unwrap();

    assert_eq!(outcome.left.source_type, "registry");
    assert_eq!(outcome.right.version.as_deref(), Some("2"));
    assert_eq!(outcome.added, vec!["references/new.md"]);
    assert!(outcome.removed.is_empty());
    assert!(outcome.changed.is_empty());
}

#[test]
fn diff_installed_copy_against_registry_current_version() {
    let mock = MockRegistryClient::with_user("alice@example.com");
    let scratch = unique_dir("diff-installed");
    let source = scratch.join("alpha");
    let target_root = scratch.join("target");
    let cache = scratch.join("cache");
    make_skill(&source, "alpha", "Use when working on alpha tasks");
    publish_approved_skill(&mock, &source, "alpha");

    let initial: SkillRef = "acme/alpha@1".parse().unwrap();
    install_remote_with_client(
        &mock,
        RemoteInstallOptions {
            skill_ref: &initial,
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
            allow_yanked: false,
        },
    )
    .unwrap();

    fs::create_dir_all(source.join("references")).unwrap();
    fs::write(source.join("references/new.md"), "new registry note\n").unwrap();
    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();
    mock.approve(&"acme/alpha".parse().unwrap(), "2").unwrap();

    let outcome = diff_installed_with_client(
        &mock,
        None,
        InstalledDiffOptions {
            json: false,
            skill_ref: "alpha",
            target: InstallTarget::Local,
            target_root: &target_root,
            quiet: true,
            allow_yanked: false,
        },
    )
    .unwrap();

    assert_eq!(outcome.left.source_type, "installed");
    assert_eq!(outcome.left.version.as_deref(), Some("1"));
    assert_eq!(outcome.right.source_type, "registry");
    assert_eq!(outcome.right.version.as_deref(), Some("2"));
    assert_eq!(outcome.added, vec!["references/new.md"]);
    assert!(outcome.removed.is_empty());
    assert!(outcome.changed.is_empty());
    assert!(!outcome.is_empty);

    let json = serde_json::to_value(&outcome).unwrap();
    assert_eq!(json["left"]["source_type"], "installed");
    assert_eq!(json["left"]["version"], "1");
    assert_eq!(json["added"][0], "references/new.md");
}

#[test]
fn diff_installed_copy_against_pinned_version_is_empty() {
    let mock = MockRegistryClient::with_user("alice@example.com");
    let scratch = unique_dir("diff-installed-pinned");
    let source = scratch.join("alpha");
    let target_root = scratch.join("target");
    let cache = scratch.join("cache");
    make_skill(&source, "alpha", "Use when working on alpha tasks");
    publish_approved_skill(&mock, &source, "alpha");

    let initial: SkillRef = "acme/alpha@1".parse().unwrap();
    install_remote_with_client(
        &mock,
        RemoteInstallOptions {
            skill_ref: &initial,
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
            allow_yanked: false,
        },
    )
    .unwrap();

    let outcome = diff_installed_with_client(
        &mock,
        None,
        InstalledDiffOptions {
            json: false,
            skill_ref: "acme/alpha@1",
            target: InstallTarget::Local,
            target_root: &target_root,
            quiet: true,
            allow_yanked: false,
        },
    )
    .unwrap();

    assert!(outcome.is_empty);
    assert_eq!(outcome.changed_count, 0);
    assert_eq!(outcome.right.version.as_deref(), Some("1"));
}

#[test]
fn diff_installed_missing_receipt_has_structured_error() {
    let mock = MockRegistryClient::new();
    let scratch = unique_dir("diff-installed-missing");
    let target_root = scratch.join("target");
    fs::create_dir_all(&target_root).unwrap();

    let err = diff_installed_with_client(
        &mock,
        None,
        InstalledDiffOptions {
            json: false,
            skill_ref: "alpha",
            target: InstallTarget::Local,
            target_root: &target_root,
            quiet: true,
            allow_yanked: false,
        },
    )
    .unwrap_err();

    let cli_err = err
        .downcast_ref::<CliError>()
        .expect("expected structured CliError");
    assert_eq!(cli_err.code, "install_receipt_missing");
    assert_eq!(cli_err.action.as_deref(), Some("diff"));
    assert_eq!(
        cli_err.next_command.as_deref(),
        Some("agentstack install list --target local")
    );
}

#[test]
fn diff_installed_rejects_mismatched_org() {
    let mock = MockRegistryClient::with_user("alice@example.com");
    let scratch = unique_dir("diff-installed-org");
    let source = scratch.join("alpha");
    let target_root = scratch.join("target");
    let cache = scratch.join("cache");
    make_skill(&source, "alpha", "Use when working on alpha tasks");
    publish_approved_skill(&mock, &source, "alpha");

    let initial: SkillRef = "acme/alpha@1".parse().unwrap();
    install_remote_with_client(
        &mock,
        RemoteInstallOptions {
            skill_ref: &initial,
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
            allow_yanked: false,
        },
    )
    .unwrap();

    let err = diff_installed_with_client(
        &mock,
        None,
        InstalledDiffOptions {
            json: false,
            skill_ref: "globex/alpha",
            target: InstallTarget::Local,
            target_root: &target_root,
            quiet: true,
            allow_yanked: false,
        },
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("came from org `acme`, not `globex`"), "{msg}");
}

#[test]
fn diff_allow_yanked_requires_pinned_registry_refs() {
    let mock = MockRegistryClient::new();
    let err = diff_with_client(
        &mock,
        None,
        DiffOptions {
            json: false,
            left: "acme/alpha",
            right: "acme/alpha@1",
            quiet: true,
            allow_yanked: true,
        },
    )
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("--allow-yanked requires explicit pinned refs")
    );
}

struct StaticPullClient {
    response: PullResponse,
}

impl RegistryClient for StaticPullClient {
    fn ping(&self) -> anyhow::Result<agentstack::registry::PingResponse> {
        unimplemented!()
    }

    fn whoami(&self) -> anyhow::Result<agentstack::registry::WhoamiResponse> {
        unimplemented!()
    }

    fn push(
        &self,
        _request: PushRequest<'_>,
    ) -> anyhow::Result<agentstack::registry::PushResponse> {
        unimplemented!()
    }

    fn pull_with_options(
        &self,
        _skill_ref: &SkillRef,
        _options: PullClientOptions,
    ) -> anyhow::Result<PullResponse> {
        Ok(self.response.clone())
    }

    fn approve(&self, _skill_ref: &SkillRef, _version: &str) -> anyhow::Result<SkillMetadata> {
        unimplemented!()
    }

    fn yank(
        &self,
        _skill_ref: &SkillRef,
        _version: &str,
        _reason: &str,
    ) -> anyhow::Result<SkillMetadata> {
        unimplemented!()
    }

    fn deprecate(
        &self,
        _skill_ref: &SkillRef,
        _version: &str,
        _reason: &str,
    ) -> anyhow::Result<SkillMetadata> {
        unimplemented!()
    }

    fn search(&self, _query: &str) -> anyhow::Result<Vec<agentstack::registry::SearchResult>> {
        unimplemented!()
    }

    fn list_remote(
        &self,
        _org: Option<&str>,
    ) -> anyhow::Result<Vec<agentstack::registry::RemoteSkill>> {
        unimplemented!()
    }

    fn list_versions(
        &self,
        _skill_ref: &SkillRef,
    ) -> anyhow::Result<Vec<agentstack::registry::VersionInfo>> {
        unimplemented!()
    }
}

#[test]
fn skill_ref_round_trip_through_workflow() {
    // The parser is unit-tested in the lib; the workflow re-parses too and
    // here we verify identical output shape.
    let r: SkillRef = "acme/code-review".parse().unwrap();
    assert_eq!(r.unversioned(), "acme/code-review");
    let r: SkillRef = "acme/code-review@1.2.3".parse().unwrap();
    assert_eq!(r.version.as_deref(), Some("1.2.3"));
    assert_eq!(r.to_string(), "acme/code-review@1.2.3");
}

#[test]
fn push_sends_expected_metadata() {
    let scratch = unique_dir("push-meta");
    let source = scratch.join("alpha");
    make_skill(&source, "alpha", "Use when working on alpha tasks");

    let mock = MockRegistryClient::with_user("octocat");
    push_with_client(
        Some(&mock),
        Some("https://registry.example.com"),
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec!["claude-code".into(), "codex".into()],
            dry_run: false,
        },
    )
    .unwrap();

    let stored = mock.pushed_metadata("acme", "alpha", "1").unwrap();
    assert_eq!(stored.name, "alpha");
    assert_eq!(stored.description, "Use when working on alpha tasks");
    assert_eq!(stored.org, "acme");
    assert_eq!(stored.visibility, Visibility::Org);
    assert_eq!(stored.team, None);
    assert_eq!(stored.version, "1");
    assert_eq!(stored.platform_tags, vec!["claude-code", "codex"]);
    assert_eq!(stored.hash.algorithm, "sha256");
    assert!(stored.created_at.is_some(), "mock fills timestamps");
    assert_eq!(stored.status, Some(VersionStatus::Candidate));
    assert_eq!(stored.current, Some(false));
    let r: SkillRef = "acme/alpha".parse().unwrap();
    let err = mock.pull(&r).unwrap_err();
    assert!(format!("{err:#}").contains("no approved/current version"));
}

#[test]
fn approve_promotes_candidate_as_current_for_default_install() {
    let scratch = unique_dir("approve-current");
    let source = scratch.join("alpha");
    make_skill(&source, "alpha", "Use when alpha");

    let mock = MockRegistryClient::new();
    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();

    let r: SkillRef = "acme/alpha".parse().unwrap();
    approve_with_client(&mock, None, &r, "1", false, false).unwrap();
    let stored = mock.pushed_metadata("acme", "alpha", "1").unwrap();
    assert_eq!(stored.status, Some(VersionStatus::Approved));
    assert_eq!(stored.current, Some(true));

    let dest_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    install_remote_with_client(
        &mock,
        RemoteInstallOptions {
            skill_ref: &r,
            dest_root: &dest_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: None,
            cache_root: Some(&cache_root),
            allow_yanked: false,
        },
    )
    .unwrap();
    assert!(dest_root.join("alpha/SKILL.md").is_file());
}

#[test]
fn approve_errors_when_response_does_not_mark_current() {
    struct BadApproveClient;

    impl RegistryClient for BadApproveClient {
        fn ping(&self) -> anyhow::Result<agentstack::registry::PingResponse> {
            unimplemented!()
        }

        fn whoami(&self) -> anyhow::Result<agentstack::registry::WhoamiResponse> {
            unimplemented!()
        }

        fn push(
            &self,
            _request: PushRequest<'_>,
        ) -> anyhow::Result<agentstack::registry::PushResponse> {
            unimplemented!()
        }

        fn pull_with_options(
            &self,
            _skill_ref: &SkillRef,
            _options: PullClientOptions,
        ) -> anyhow::Result<PullResponse> {
            unimplemented!()
        }

        fn approve(&self, _skill_ref: &SkillRef, _version: &str) -> anyhow::Result<SkillMetadata> {
            Ok(SkillMetadata {
                name: "alpha".to_string(),
                description: "Use when alpha".to_string(),
                org: "acme".to_string(),
                owner_email: None,
                team: None,
                visibility: Visibility::Org,
                version: "1".to_string(),
                hash: PackageHash::sha256_of(b"alpha"),
                platform_tags: vec![],
                created_at: None,
                updated_at: None,
                install_count: None,
                last_installed_at: None,
                status: Some(VersionStatus::Approved),
                current: None,
                yanked_at: None,
                yank_reason: None,
                deprecated_at: None,
                deprecation_reason: None,
                audit_event_id: None,
            })
        }

        fn yank(
            &self,
            _skill_ref: &SkillRef,
            _version: &str,
            _reason: &str,
        ) -> anyhow::Result<SkillMetadata> {
            unimplemented!()
        }

        fn deprecate(
            &self,
            _skill_ref: &SkillRef,
            _version: &str,
            _reason: &str,
        ) -> anyhow::Result<SkillMetadata> {
            unimplemented!()
        }

        fn search(&self, _query: &str) -> anyhow::Result<Vec<agentstack::registry::SearchResult>> {
            unimplemented!()
        }

        fn list_remote(
            &self,
            _org: Option<&str>,
        ) -> anyhow::Result<Vec<agentstack::registry::RemoteSkill>> {
            unimplemented!()
        }

        fn list_versions(
            &self,
            _skill_ref: &SkillRef,
        ) -> anyhow::Result<Vec<agentstack::registry::VersionInfo>> {
            unimplemented!()
        }
    }

    let r: SkillRef = "acme/alpha".parse().unwrap();
    let err = approve_with_client(&BadApproveClient, None, &r, "1", false, false).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("current=true"), "got: {msg}");
}

#[test]
fn push_invalid_skill_does_not_call_registry() {
    let scratch = unique_dir("push-invalid");
    let source = scratch.join("broken");
    fs::create_dir_all(&source).unwrap(); // No SKILL.md.

    let mock = MockRegistryClient::with_user("octocat");
    let err = push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Private,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("not a valid skill"),
        "expected validation failure, got: {msg}"
    );
    // Mock must not have stored anything.
    assert!(mock.pushed_metadata("acme", "broken", "1").is_none());
}

#[test]
fn dry_run_does_not_call_registry() {
    let scratch = unique_dir("push-dry-run");
    let source = scratch.join("alpha");
    make_skill(&source, "alpha", "Use when alpha");

    let outcome = push_with_client(
        None,
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Private,
            team: None,
            platforms: vec!["codex".into()],
            dry_run: true,
        },
    )
    .unwrap();

    assert!(outcome.dry_run);
}

#[test]
fn dry_run_runs_validate_and_pack() {
    let scratch = unique_dir("push-dry-run-invalid");
    let source = scratch.join("broken");
    fs::create_dir_all(&source).unwrap(); // No SKILL.md.

    let dry_run_err = push_with_client(
        None,
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Private,
            team: None,
            platforms: vec![],
            dry_run: true,
        },
    )
    .unwrap_err();

    let live = MockRegistryClient::new();
    let live_err = push_with_client(
        Some(&live),
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Private,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap_err();

    assert_eq!(format!("{dry_run_err:#}"), format!("{live_err:#}"));
    assert!(format!("{dry_run_err:#}").contains("not a valid skill"));
}

#[test]
fn push_rejects_name_directory_mismatch() {
    let scratch = unique_dir("push-name-mismatch");
    let source = scratch.join("wrong-dir");
    make_skill(&source, "alpha", "Use when alpha");

    let mock = MockRegistryClient::new();
    let err = push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Private,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("not a valid skill") && msg.contains("wrong-dir"),
        "expected validation failure, got: {msg}"
    );
    assert!(mock.pushed_metadata("acme", "alpha", "1").is_none());
}

#[test]
fn push_invalid_org_rejects() {
    let scratch = unique_dir("push-bad-org");
    let source = scratch.join("alpha");
    make_skill(&source, "alpha", "Use when alpha");

    let mock = MockRegistryClient::new();
    let err = push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &source,
            org: "Bad_Org",
            visibility: Visibility::Private,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("invalid --org"), "got: {msg}");
}

#[test]
fn skill_export_writes_expected_files() {
    // First, push a skill so the mock has something to return.
    let scratch = unique_dir("export-roundtrip");
    let source = scratch.join("beta");
    make_skill(&source, "beta", "Use when working on beta");
    fs::create_dir_all(source.join("references")).unwrap();
    fs::create_dir_all(source.join("examples")).unwrap();
    fs::write(source.join("references/notes.md"), "# Beta notes").unwrap();

    let mock = MockRegistryClient::new();
    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();

    let dest_parent = scratch.join("downloaded");
    let dest = dest_parent.join("beta");
    let r: SkillRef = "acme/beta@1".parse().unwrap();
    registry_export_with_client(
        &mock,
        None,
        ExportOptions {
            json: false,
            skill_ref: &r,
            out: Some(&dest_parent),
            force: false,
            quiet: false,
            dry_run: false,
            allow_yanked: false,
        },
    )
    .unwrap();

    assert!(
        dest.join("SKILL.md").is_file(),
        "SKILL.md should be unpacked"
    );
    assert!(
        dest.join("references/notes.md").is_file(),
        "notes.md should be unpacked"
    );
    let body = fs::read_to_string(dest.join("SKILL.md")).unwrap();
    assert!(body.contains("name: beta"));
    assert_valid_and_lints_clean(&dest);
}

#[test]
fn skill_export_dry_run_does_not_write_to_disk() {
    let scratch = unique_dir("export-dryrun");
    let source = scratch.join("delta");
    make_skill(&source, "delta", "Use when working on delta");

    let mock = MockRegistryClient::new();
    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();

    let dest_parent = scratch.join("downloaded");
    let dest = dest_parent.join("delta");
    let r: SkillRef = "acme/delta@1".parse().unwrap();
    registry_export_with_client(
        &mock,
        None,
        ExportOptions {
            json: false,
            skill_ref: &r,
            out: Some(&dest_parent),
            force: false,
            quiet: true,
            dry_run: true,
            allow_yanked: false,
        },
    )
    .unwrap();

    assert!(
        !dest.exists(),
        "dry-run must not create the destination directory"
    );
}

#[test]
fn skill_export_dry_run_detects_overwrite_conflict() {
    let scratch = unique_dir("export-dryrun-conflict");
    let source = scratch.join("epsilon");
    make_skill(&source, "epsilon", "Use when epsilon");

    let mock = MockRegistryClient::new();
    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();

    let dest_parent = scratch.join("out");
    let dest = dest_parent.join("epsilon");
    fs::create_dir_all(&dest).unwrap();
    fs::write(dest.join("preexisting.txt"), "user data").unwrap();

    let r: SkillRef = "acme/epsilon@1".parse().unwrap();
    let err = registry_export_with_client(
        &mock,
        None,
        ExportOptions {
            json: false,
            skill_ref: &r,
            out: Some(&dest_parent),
            force: false,
            quiet: true,
            dry_run: true,
            allow_yanked: false,
        },
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("refusing to overwrite"),
        "expected overwrite refusal, got: {msg}"
    );
    assert!(
        dest.join("preexisting.txt").is_file(),
        "user file must be preserved during dry-run"
    );
}

#[test]
fn skill_export_refuses_overwrite_without_force() {
    let scratch = unique_dir("export-overwrite");
    let source = scratch.join("gamma");
    make_skill(&source, "gamma", "Use when gamma");

    let mock = MockRegistryClient::new();
    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Private,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();

    let dest_parent = scratch.join("out");
    let dest = dest_parent.join("gamma");
    fs::create_dir_all(&dest).unwrap();
    fs::write(dest.join("preexisting.txt"), "user data").unwrap();

    let r: SkillRef = "acme/gamma@1".parse().unwrap();
    let err = registry_export_with_client(
        &mock,
        None,
        ExportOptions {
            json: false,
            skill_ref: &r,
            out: Some(&dest_parent),
            force: false,
            quiet: false,
            dry_run: false,
            allow_yanked: false,
        },
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("refusing to overwrite"),
        "expected overwrite refusal, got: {msg}"
    );
    assert!(
        dest.join("preexisting.txt").is_file(),
        "user file must be preserved"
    );
}

#[test]
fn skill_export_force_replaces_existing_destination() {
    let scratch = unique_dir("export-force-replace");
    let source = scratch.join("gamma");
    make_skill(&source, "gamma", "Use when gamma");

    let mock = MockRegistryClient::new();
    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Private,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();

    let dest_parent = scratch.join("out");
    let dest = dest_parent.join("gamma");
    fs::create_dir_all(&dest).unwrap();
    fs::write(dest.join("stale.txt"), "old data").unwrap();

    let r: SkillRef = "acme/gamma@1".parse().unwrap();
    registry_export_with_client(
        &mock,
        None,
        ExportOptions {
            json: false,
            skill_ref: &r,
            out: Some(&dest_parent),
            force: true,
            quiet: false,
            dry_run: false,
            allow_yanked: false,
        },
    )
    .unwrap();

    assert!(dest.join("SKILL.md").is_file());
    assert!(!dest.join("stale.txt").exists());
}

#[test]
fn skill_export_fails_when_hash_mismatches_archive() {
    // Seed the mock with metadata that says one hash but archive bytes that
    // hash to something else. The export workflow must catch the mismatch and
    // refuse to write to disk.
    let mock = MockRegistryClient::new();

    let bogus_metadata = SkillMetadata {
        name: "alpha".to_string(),
        description: "Use when alpha".to_string(),
        org: "acme".to_string(),
        owner_email: None,
        team: None,
        visibility: Visibility::Org,
        version: "1".to_string(),
        // Hash of *different* bytes:
        hash: PackageHash::sha256_of(b"the-real-archive-bytes"),
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
    };
    mock.seed(
        bogus_metadata.clone(),
        b"these-bytes-will-not-match".to_vec(),
    );

    let scratch = unique_dir("export-bad-hash");
    let dest = scratch.join("out");
    let r: SkillRef = "acme/alpha@1".parse().unwrap();

    let err = registry_export_with_client(
        &mock,
        None,
        ExportOptions {
            json: false,
            skill_ref: &r,
            out: Some(&dest),
            force: false,
            quiet: false,
            dry_run: false,
            allow_yanked: false,
        },
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("hash mismatch"),
        "expected hash mismatch error, got: {msg}"
    );
    // The destination must not have been created.
    assert!(!dest.exists() || fs::read_dir(&dest).unwrap().next().is_none());
}

#[test]
fn registry_pullresponse_shape_documented_in_contract() {
    // Sanity: confirm PullResponse exposes both metadata and bytes (the
    // contract documented in docs/API_CONTRACT.md).
    let mock = MockRegistryClient::new();
    let scratch = unique_dir("export-shape");
    let source = scratch.join("delta");
    make_skill(&source, "delta", "Use when delta");
    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Private,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();

    mock.approve(&"acme/delta".parse().unwrap(), "1").unwrap();
    let r: SkillRef = "acme/delta".parse().unwrap();
    let resp: PullResponse = mock.pull(&r).unwrap();
    assert_eq!(resp.metadata.org, "acme");
    assert_eq!(resp.metadata.name, "delta");
    assert_eq!(resp.metadata.version, "1");
    assert!(!resp.archive.is_empty());
}

#[test]
fn yanked_version_is_hidden_from_discovery_and_requires_allow_yanked_to_pull() {
    let mock = MockRegistryClient::new();
    let scratch = unique_dir("yank-lifecycle");
    let source = scratch.join("alpha");
    make_skill(&source, "alpha", "Use when alpha");
    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();
    let skill_ref: SkillRef = "acme/alpha".parse().unwrap();
    mock.approve(&skill_ref, "1").unwrap();

    yank_with_client(
        &mock,
        YankOptions {
            registry_url: Some("mock://registry"),
            skill_ref: &skill_ref,
            version: "1",
            reason: "bad archive",
            action: YankAction::Yank,
            json: true,
            quiet: false,
        },
    )
    .unwrap();

    let versions = mock.list_versions(&skill_ref).unwrap();
    assert_eq!(versions[0].version, "1");
    assert!(versions[0].yanked_at.is_some());
    assert_eq!(versions[0].yank_reason.as_deref(), Some("bad archive"));
    assert!(mock.search("alpha").unwrap().is_empty());
    assert!(mock.list_remote(Some("acme")).unwrap().is_empty());

    let pinned: SkillRef = "acme/alpha@1".parse().unwrap();
    let blocked = registry_export_with_client(
        &mock,
        None,
        ExportOptions {
            json: false,
            skill_ref: &pinned,
            out: Some(&scratch.join("blocked")),
            force: false,
            quiet: true,
            dry_run: false,
            allow_yanked: false,
        },
    )
    .unwrap_err();
    let msg = format!("{blocked:#}");
    assert!(msg.contains("was yanked: bad archive"), "got: {msg}");

    let allowed_out = scratch.join("allowed");
    registry_export_with_client(
        &mock,
        None,
        ExportOptions {
            json: false,
            skill_ref: &pinned,
            out: Some(&allowed_out),
            force: false,
            quiet: true,
            dry_run: false,
            allow_yanked: true,
        },
    )
    .unwrap();
    assert!(allowed_out.join("alpha/SKILL.md").is_file());
}

#[test]
fn discovery_falls_back_when_newest_upload_is_yanked() {
    let mock = MockRegistryClient::new();
    let scratch = unique_dir("yank-newest-fallback");
    let source = scratch.join("alpha");
    make_skill(&source, "alpha", "Use when alpha");
    for _ in 0..11 {
        push_with_client(
            Some(&mock),
            None,
            PushOptions {
                source: &source,
                org: "acme",
                visibility: Visibility::Org,
                team: None,
                platforms: vec![],
                dry_run: false,
            },
        )
        .unwrap();
    }
    let skill_ref: SkillRef = "acme/alpha".parse().unwrap();
    mock.approve(&skill_ref, "10").unwrap();
    mock.yank(&skill_ref, "11", "bad candidate").unwrap();

    let search = mock.search("alpha").unwrap();
    assert_eq!(search.len(), 1);
    assert_eq!(search[0].latest_version, "10");
    assert_eq!(search[0].current_version.as_deref(), Some("10"));

    let remote = mock.list_remote(Some("acme")).unwrap();
    assert_eq!(remote.len(), 1);
    assert_eq!(remote[0].latest_version, "10");
    assert_eq!(remote[0].current_version.as_deref(), Some("10"));
}

#[test]
fn deprecated_version_records_reason_but_remains_pullable() {
    let mock = MockRegistryClient::new();
    let scratch = unique_dir("deprecated-lifecycle");
    let source = scratch.join("alpha");
    make_skill(&source, "alpha", "Use when alpha");
    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();
    let skill_ref: SkillRef = "acme/alpha".parse().unwrap();
    mock.approve(&skill_ref, "1").unwrap();

    yank_with_client(
        &mock,
        YankOptions {
            registry_url: Some("mock://registry"),
            skill_ref: &skill_ref,
            version: "1",
            reason: "superseded",
            action: YankAction::Deprecate,
            json: true,
            quiet: false,
        },
    )
    .unwrap();

    let versions = mock.list_versions(&skill_ref).unwrap();
    assert!(versions[0].deprecated_at.is_some());
    assert_eq!(
        versions[0].deprecation_reason.as_deref(),
        Some("superseded")
    );

    let out = scratch.join("exported");
    let pinned: SkillRef = "acme/alpha@1".parse().unwrap();
    registry_export_with_client(
        &mock,
        None,
        ExportOptions {
            json: false,
            skill_ref: &pinned,
            out: Some(&out),
            force: false,
            quiet: true,
            dry_run: false,
            allow_yanked: false,
        },
    )
    .unwrap();
    assert!(out.join("alpha/SKILL.md").is_file());
}

#[test]
fn remote_install_writes_registry_receipt_and_cache_entry() {
    let scratch = unique_dir("remote-install-receipt");
    let source = scratch.join("installable");
    make_skill(&source, "installable", "Use when installing from registry");

    let mock = MockRegistryClient::with_user("octocat");
    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();
    mock.approve(&"acme/installable".parse().unwrap(), "1")
        .unwrap();

    let dest_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    let r: SkillRef = "acme/installable".parse().unwrap();
    let report = install_remote_with_client(
        &mock,
        RemoteInstallOptions {
            skill_ref: &r,
            dest_root: &dest_root,
            target: "local",
            force: false,
            registry_url: Some("https://registry.example.com"),
            installed_by: Some("octocat".into()),
            cache_root: Some(&cache_root),
            allow_yanked: false,
        },
    )
    .unwrap();

    let installed = dest_root.join("installable");
    assert!(installed.join("SKILL.md").is_file());
    assert!(report.cache_entry.package_path.is_file());
    assert!(
        !report.install.warnings.is_empty(),
        "lint warnings should be collected before install"
    );

    let receipt = receipt(&installed);
    assert_eq!(receipt["schema_version"].as_u64(), Some(1));
    assert_eq!(receipt["skill_name"].as_str(), Some("installable"));
    assert_eq!(receipt["source_type"].as_str(), Some("registry"));
    assert_eq!(receipt["source_ref"].as_str(), Some("acme/installable"));
    assert_eq!(
        receipt["registry_url"].as_str(),
        Some("https://registry.example.com")
    );
    assert_eq!(receipt["org"].as_str(), Some("acme"));
    assert_eq!(receipt["version"].as_str(), Some("1"));
    assert_eq!(receipt["target"].as_str(), Some("local"));
    assert_eq!(receipt["installed_by"].as_str(), Some("octocat"));
    let expected_hash = format!("sha256:{}", report.metadata.hash.hex);
    assert_eq!(receipt["hash"].as_str(), Some(expected_hash.as_str()));
    assert!(
        receipt["content_hash"]
            .as_str()
            .is_some_and(|hash| hash.starts_with("sha256:")),
        "receipt should record installed content hash: {receipt}"
    );
}

#[test]
fn stack_install_installs_child_skills_and_writes_receipts() {
    let scratch = unique_dir("stack-install");
    let src_incident = scratch.join("src/incident-runbook");
    let src_api = scratch.join("src/api-review-checklist");
    make_skill(
        &src_incident,
        "incident-runbook",
        "Use when handling incidents",
    );
    make_skill(&src_api, "api-review-checklist", "Use when reviewing APIs");

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &src_incident, "incident-runbook");
    publish_approved_skill(&mock, &src_api, "api-review-checklist");
    mock.create_stack(
        "acme",
        "engineering-default",
        "Engineering Default",
        "",
        Visibility::Org,
        None,
    )
    .unwrap();
    mock.upsert_stack_item(
        "acme",
        "engineering-default",
        "incident-runbook",
        VersionPolicy::Current,
        None,
    )
    .unwrap();
    mock.upsert_stack_item(
        "acme",
        "engineering-default",
        "api-review-checklist",
        VersionPolicy::Current,
        None,
    )
    .unwrap();

    let target_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    let report = install_stack_with_client(
        &mock,
        StackInstallOptions {
            org: "acme",
            stack: "engineering-default",
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("https://registry.example.com"),
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();

    assert_eq!(report.installed.len(), 2);
    assert!(target_root.join("incident-runbook/SKILL.md").is_file());
    assert!(target_root.join("api-review-checklist/SKILL.md").is_file());
    assert!(report.stack_receipt_path.is_file());

    let stack_receipt = fs::read_to_string(&report.stack_receipt_path).unwrap();
    assert!(!stack_receipt.contains("secret-token"));
    let stack_json: serde_json::Value = serde_json::from_str(&stack_receipt).unwrap();
    assert_eq!(stack_json["kind"].as_str(), Some("stack"));
    assert_eq!(stack_json["org"].as_str(), Some("acme"));
    assert_eq!(stack_json["stack"].as_str(), Some("engineering-default"));
    assert_eq!(
        stack_json["registry_url"].as_str(),
        Some("https://registry.example.com")
    );
    assert_eq!(stack_json["target"].as_str(), Some("local"));
    assert_eq!(stack_json["items"].as_array().unwrap().len(), 2);

    let child_receipt = receipt(&target_root.join("incident-runbook"));
    assert_eq!(child_receipt["source_type"].as_str(), Some("registry"));
    assert_eq!(
        child_receipt["installed_via"]["kind"].as_str(),
        Some("stack")
    );
    assert_eq!(child_receipt["installed_via"]["org"].as_str(), Some("acme"));
    assert_eq!(
        child_receipt["installed_via"]["stack"].as_str(),
        Some("engineering-default")
    );
    assert!(!child_receipt.to_string().contains("secret-token"));

    let second = install_stack_with_client(
        &mock,
        StackInstallOptions {
            org: "acme",
            stack: "engineering-default",
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("https://registry.example.com"),
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();
    assert!(
        second.installed.iter().all(|item| item.overwrote_existing),
        "reinstalling the same registry stack should be an identity-matched update"
    );
}

#[test]
fn stack_install_merges_shared_child_stack_referrers() {
    let scratch = unique_dir("stack-install-shared-child");
    let shared = scratch.join("src/shared-skill");
    make_skill(&shared, "shared-skill", "Use when shared");

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &shared, "shared-skill");
    for stack in ["stack-a", "stack-b"] {
        mock.create_stack("acme", stack, stack, "", Visibility::Org, None)
            .unwrap();
        mock.upsert_stack_item("acme", stack, "shared-skill", VersionPolicy::Current, None)
            .unwrap();
    }

    let target_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    for stack in ["stack-a", "stack-b"] {
        install_stack_with_client(
            &mock,
            StackInstallOptions {
                org: "acme",
                stack,
                dest_root: &target_root,
                target: "local",
                force: false,
                registry_url: Some("mock://registry"),
                installed_by: Some("octocat".to_string()),
                cache_root: Some(&cache_root),
            },
        )
        .unwrap();
    }

    let child_receipt = receipt(&target_root.join("shared-skill"));
    let refs = child_receipt["installed_via_stacks"].as_array().unwrap();
    assert_eq!(refs.len(), 2);
    assert!(refs.iter().any(|via| via["stack"] == "stack-a"));
    assert!(refs.iter().any(|via| via["stack"] == "stack-b"));
    assert!(stack_receipt(&target_root, "acme", "stack-a")["items"].is_array());
    assert!(stack_receipt(&target_root, "acme", "stack-b")["items"].is_array());
}

#[test]
fn stack_install_refuses_to_adopt_direct_skill_without_force() {
    let scratch = unique_dir("stack-install-direct-child");
    let source = scratch.join("src/direct-skill");
    make_skill(
        &source,
        "direct-skill",
        "Use when direct skill ownership matters",
    );

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &source, "direct-skill");
    mock.create_stack(
        "acme",
        "engineering-default",
        "Engineering Default",
        "",
        Visibility::Org,
        None,
    )
    .unwrap();
    mock.upsert_stack_item(
        "acme",
        "engineering-default",
        "direct-skill",
        VersionPolicy::Current,
        None,
    )
    .unwrap();

    let target_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    let skill_ref: SkillRef = "acme/direct-skill".parse().unwrap();
    install_remote_with_client(
        &mock,
        RemoteInstallOptions {
            skill_ref: &skill_ref,
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
            allow_yanked: false,
        },
    )
    .unwrap();

    let err = install_stack_with_client(
        &mock,
        StackInstallOptions {
            org: "acme",
            stack: "engineering-default",
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap_err();

    let msg = format!("{err:#}");
    assert!(
        msg.contains("refusing to adopt existing direct install"),
        "got: {msg}"
    );
    assert!(msg.contains("--force"), "got: {msg}");
    let child_receipt = receipt(&target_root.join("direct-skill"));
    assert!(child_receipt["installed_via"].is_null());
}

#[test]
fn stack_install_refuses_shared_child_version_conflict() {
    let scratch = unique_dir("stack-install-shared-conflict");
    let shared = scratch.join("src/shared-skill");
    make_skill(&shared, "shared-skill", "Use when shared");

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &shared, "shared-skill");
    mock.create_stack("acme", "stack-a", "Stack A", "", Visibility::Org, None)
        .unwrap();
    mock.upsert_stack_item(
        "acme",
        "stack-a",
        "shared-skill",
        VersionPolicy::Pinned,
        Some("1"),
    )
    .unwrap();

    let target_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    install_stack_with_client(
        &mock,
        StackInstallOptions {
            org: "acme",
            stack: "stack-a",
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();

    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &shared,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();
    mock.approve(&"acme/shared-skill".parse().unwrap(), "2")
        .unwrap();
    mock.create_stack("acme", "stack-b", "Stack B", "", Visibility::Org, None)
        .unwrap();
    mock.upsert_stack_item(
        "acme",
        "stack-b",
        "shared-skill",
        VersionPolicy::Current,
        None,
    )
    .unwrap();

    let err = install_stack_with_client(
        &mock,
        StackInstallOptions {
            org: "acme",
            stack: "stack-b",
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("shared stack-owned skill `shared-skill`"),
        "got: {msg}"
    );
}

#[test]
fn concurrent_same_target_stack_installs_serialize_cleanly() {
    let scratch = unique_dir("stack-install-concurrent");
    let src_incident = scratch.join("src/incident-runbook");
    let src_api = scratch.join("src/api-review-checklist");
    make_skill(
        &src_incident,
        "incident-runbook",
        "Use when handling incidents",
    );
    make_skill(&src_api, "api-review-checklist", "Use when reviewing APIs");

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &src_incident, "incident-runbook");
    publish_approved_skill(&mock, &src_api, "api-review-checklist");
    mock.create_stack(
        "acme",
        "engineering-default",
        "Engineering Default",
        "",
        Visibility::Org,
        None,
    )
    .unwrap();
    for skill in ["incident-runbook", "api-review-checklist"] {
        mock.upsert_stack_item(
            "acme",
            "engineering-default",
            skill,
            VersionPolicy::Current,
            None,
        )
        .unwrap();
    }

    let target_root = scratch.join("target");
    thread::scope(|scope| {
        let mut handles = Vec::new();
        for i in 0..5 {
            let mock = &mock;
            let target_root = &target_root;
            let cache_root = scratch.join(format!("cache-{i}"));
            handles.push(scope.spawn(move || {
                install_stack_with_client(
                    mock,
                    StackInstallOptions {
                        org: "acme",
                        stack: "engineering-default",
                        dest_root: target_root,
                        target: "local",
                        force: true,
                        registry_url: Some("mock://registry"),
                        installed_by: Some("octocat".to_string()),
                        cache_root: Some(&cache_root),
                    },
                )
            }));
        }
        for handle in handles {
            let report = handle.join().unwrap().unwrap();
            assert_eq!(report.installed.len(), 2);
        }
    });

    assert!(target_root.join("incident-runbook/SKILL.md").is_file());
    assert!(target_root.join("api-review-checklist/SKILL.md").is_file());
    assert!(receipt(&target_root.join("incident-runbook"))["installed_via"].is_object());
    assert!(receipt(&target_root.join("api-review-checklist"))["installed_via"].is_object());
    let stack_json = stack_receipt(&target_root, "acme", "engineering-default");
    assert_eq!(stack_json["items"].as_array().unwrap().len(), 2);
    assert!(!target_root.join(".agentstack-install.lock").exists());
}

#[test]
fn stack_install_bad_child_aborts_before_committing_any_skills() {
    let scratch = unique_dir("stack-install-bad-child");
    let src_good = scratch.join("src/good-skill");
    let src_bad = scratch.join("src/bad-skill");
    make_skill(&src_good, "good-skill", "Use when good");
    make_skill(&src_bad, "bad-skill", "Use when bad");

    let mock = MockRegistryClient::new();
    publish_approved_skill(&mock, &src_good, "good-skill");

    let built_bad = build_skill_package(&src_bad).unwrap();
    mock.seed(
        SkillMetadata {
            name: "bad-skill".to_string(),
            description: "Use when bad".to_string(),
            org: "acme".to_string(),
            owner_email: None,
            team: None,
            visibility: Visibility::Org,
            version: "1".to_string(),
            hash: PackageHash::sha256_of(b"not the archive"),
            platform_tags: vec![],
            created_at: Some("2026-01-01T00:00:00Z".to_string()),
            updated_at: Some("2026-01-01T00:00:00Z".to_string()),
            install_count: None,
            last_installed_at: None,
            status: Some(VersionStatus::Approved),
            current: Some(true),
            yanked_at: None,
            yank_reason: None,
            deprecated_at: None,
            deprecation_reason: None,
            audit_event_id: None,
        },
        built_bad.bytes,
    );
    mock.create_stack(
        "acme",
        "engineering-default",
        "Engineering Default",
        "",
        Visibility::Org,
        None,
    )
    .unwrap();
    mock.upsert_stack_item(
        "acme",
        "engineering-default",
        "good-skill",
        VersionPolicy::Current,
        None,
    )
    .unwrap();
    mock.upsert_stack_item(
        "acme",
        "engineering-default",
        "bad-skill",
        VersionPolicy::Current,
        None,
    )
    .unwrap();

    let target_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    let err = install_stack_with_client(
        &mock,
        StackInstallOptions {
            org: "acme",
            stack: "engineering-default",
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: None,
            cache_root: Some(&cache_root),
        },
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("hash mismatch"), "got: {msg}");
    assert!(!target_root.join("good-skill").exists());
    assert!(!target_root.join("bad-skill").exists());
    assert!(!target_root.join(".agentstack-stacks").exists());
    assert!(!target_root.join(".agentstack-install.lock").exists());
    let leftovers = fs::read_dir(&target_root)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".agentstack-stack-install-")
        })
        .count();
    assert_eq!(
        leftovers, 0,
        "failed stack install should clean staging dirs"
    );
}

#[test]
fn stack_update_membership_add_installs_new_skill() {
    let scratch = unique_dir("stack-update-add");
    let src_incident = scratch.join("src/incident-runbook");
    let src_api = scratch.join("src/api-review-checklist");
    make_skill(
        &src_incident,
        "incident-runbook",
        "Use when handling incidents",
    );
    make_skill(&src_api, "api-review-checklist", "Use when reviewing APIs");

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &src_incident, "incident-runbook");
    publish_approved_skill(&mock, &src_api, "api-review-checklist");
    mock.create_stack(
        "acme",
        "engineering-default",
        "Engineering Default",
        "",
        Visibility::Org,
        None,
    )
    .unwrap();
    mock.upsert_stack_item(
        "acme",
        "engineering-default",
        "incident-runbook",
        VersionPolicy::Current,
        None,
    )
    .unwrap();

    let target_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    install_stack_with_client(
        &mock,
        StackInstallOptions {
            org: "acme",
            stack: "engineering-default",
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();

    mock.upsert_stack_item(
        "acme",
        "engineering-default",
        "api-review-checklist",
        VersionPolicy::Current,
        None,
    )
    .unwrap();

    let outcome = update_stack_with_client(
        &mock,
        StackUpdateOptions {
            json: false,
            stack: "engineering-default",
            target: InstallTarget::Local,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: false,
            force: false,
            prune: false,
            quiet: true,
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();

    assert!(outcome.updated);
    assert_eq!(
        outcome
            .added
            .iter()
            .map(|item| item.skill.as_str())
            .collect::<Vec<_>>(),
        vec!["api-review-checklist"]
    );
    assert!(target_root.join("api-review-checklist/SKILL.md").is_file());
    let stack_json = stack_receipt(&target_root, "acme", "engineering-default");
    assert_eq!(stack_json["items"].as_array().unwrap().len(), 2);
    let child_receipt = receipt(&target_root.join("api-review-checklist"));
    assert_eq!(
        child_receipt["installed_via"]["stack"].as_str(),
        Some("engineering-default")
    );
}

#[test]
fn concurrent_same_target_stack_updates_serialize_cleanly() {
    let scratch = unique_dir("stack-update-concurrent");
    let src_incident = scratch.join("src/incident-runbook");
    let src_api = scratch.join("src/api-review-checklist");
    make_skill(
        &src_incident,
        "incident-runbook",
        "Use when handling incidents",
    );
    make_skill(&src_api, "api-review-checklist", "Use when reviewing APIs");

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &src_incident, "incident-runbook");
    publish_approved_skill(&mock, &src_api, "api-review-checklist");
    mock.create_stack(
        "acme",
        "engineering-default",
        "Engineering Default",
        "",
        Visibility::Org,
        None,
    )
    .unwrap();
    mock.upsert_stack_item(
        "acme",
        "engineering-default",
        "incident-runbook",
        VersionPolicy::Current,
        None,
    )
    .unwrap();

    let target_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    install_stack_with_client(
        &mock,
        StackInstallOptions {
            org: "acme",
            stack: "engineering-default",
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();

    mock.upsert_stack_item(
        "acme",
        "engineering-default",
        "api-review-checklist",
        VersionPolicy::Current,
        None,
    )
    .unwrap();

    thread::scope(|scope| {
        let mut handles = Vec::new();
        for i in 0..5 {
            let mock = &mock;
            let target_root = &target_root;
            let cache_root = scratch.join(format!("update-cache-{i}"));
            handles.push(scope.spawn(move || {
                update_stack_with_client(
                    mock,
                    StackUpdateOptions {
                        json: false,
                        stack: "engineering-default",
                        target: InstallTarget::Local,
                        target_root,
                        registry_url: Some("mock://registry"),
                        check: false,
                        force: false,
                        prune: false,
                        quiet: true,
                        installed_by: Some("octocat".to_string()),
                        cache_root: Some(&cache_root),
                    },
                )
            }));
        }
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
    });

    assert!(target_root.join("incident-runbook/SKILL.md").is_file());
    assert!(target_root.join("api-review-checklist/SKILL.md").is_file());
    let stack_json = stack_receipt(&target_root, "acme", "engineering-default");
    assert_eq!(stack_json["items"].as_array().unwrap().len(), 2);
    assert!(receipt(&target_root.join("incident-runbook"))["installed_via"].is_object());
    assert!(receipt(&target_root.join("api-review-checklist"))["installed_via"].is_object());
    assert!(!target_root.join(".agentstack-install.lock").exists());
}

#[test]
fn stack_update_check_reports_removed_item_without_pruning() {
    let scratch = unique_dir("stack-update-remove-check");
    let src_incident = scratch.join("src/incident-runbook");
    let src_api = scratch.join("src/api-review-checklist");
    make_skill(
        &src_incident,
        "incident-runbook",
        "Use when handling incidents",
    );
    make_skill(&src_api, "api-review-checklist", "Use when reviewing APIs");

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &src_incident, "incident-runbook");
    publish_approved_skill(&mock, &src_api, "api-review-checklist");
    mock.create_stack(
        "acme",
        "engineering-default",
        "Engineering Default",
        "",
        Visibility::Org,
        None,
    )
    .unwrap();
    for skill in ["incident-runbook", "api-review-checklist"] {
        mock.upsert_stack_item(
            "acme",
            "engineering-default",
            skill,
            VersionPolicy::Current,
            None,
        )
        .unwrap();
    }

    let target_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    install_stack_with_client(
        &mock,
        StackInstallOptions {
            org: "acme",
            stack: "engineering-default",
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();

    let pulls_before_check = mock.pull_count();
    mock.remove_stack_item("acme", "engineering-default", "api-review-checklist")
        .unwrap();
    let outcome = update_stack_with_client(
        &mock,
        StackUpdateOptions {
            json: false,
            stack: "engineering-default",
            target: InstallTarget::Local,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: true,
            force: false,
            prune: false,
            quiet: true,
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();

    assert!(!outcome.updated);
    assert_eq!(
        mock.pull_count(),
        pulls_before_check,
        "check mode should resolve stack metadata without downloading archives"
    );
    assert_eq!(
        outcome
            .removed
            .iter()
            .map(|item| item.skill.as_str())
            .collect::<Vec<_>>(),
        vec!["api-review-checklist"]
    );
    assert!(target_root.join("api-review-checklist/SKILL.md").is_file());
    let stack_json = stack_receipt(&target_root, "acme", "engineering-default");
    assert_eq!(
        stack_json["items"].as_array().unwrap().len(),
        2,
        "check mode must not rewrite receipts"
    );
}

#[test]
fn stack_update_noop_does_not_pull_archives() {
    let scratch = unique_dir("stack-update-noop-no-pull");
    let src = scratch.join("src/incident-runbook");
    make_skill(&src, "incident-runbook", "Use when handling incidents");

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &src, "incident-runbook");
    mock.create_stack(
        "acme",
        "engineering-default",
        "Engineering Default",
        "",
        Visibility::Org,
        None,
    )
    .unwrap();
    mock.upsert_stack_item(
        "acme",
        "engineering-default",
        "incident-runbook",
        VersionPolicy::Current,
        None,
    )
    .unwrap();

    let target_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    install_stack_with_client(
        &mock,
        StackInstallOptions {
            org: "acme",
            stack: "engineering-default",
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();

    let pulls_before_update = mock.pull_count();
    let resolves_before_update = mock.resolve_stack_count();
    let outcome = update_stack_with_client(
        &mock,
        StackUpdateOptions {
            json: false,
            stack: "engineering-default",
            target: InstallTarget::Local,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: false,
            force: false,
            prune: false,
            quiet: true,
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();

    assert!(!outcome.updated);
    assert_eq!(
        mock.resolve_stack_count(),
        resolves_before_update + 1,
        "noop update should only resolve the stack once"
    );
    assert_eq!(
        mock.pull_count(),
        pulls_before_update,
        "noop update should not download archives"
    );
}

#[test]
fn stack_update_prune_removes_safe_items_and_refuses_independent_child() {
    let scratch = unique_dir("stack-update-prune");
    let src_incident = scratch.join("src/incident-runbook");
    let src_api = scratch.join("src/api-review-checklist");
    make_skill(
        &src_incident,
        "incident-runbook",
        "Use when handling incidents",
    );
    make_skill(&src_api, "api-review-checklist", "Use when reviewing APIs");

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &src_incident, "incident-runbook");
    publish_approved_skill(&mock, &src_api, "api-review-checklist");
    mock.create_stack(
        "acme",
        "engineering-default",
        "Engineering Default",
        "",
        Visibility::Org,
        None,
    )
    .unwrap();
    for skill in ["incident-runbook", "api-review-checklist"] {
        mock.upsert_stack_item(
            "acme",
            "engineering-default",
            skill,
            VersionPolicy::Current,
            None,
        )
        .unwrap();
    }

    let target_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    install_stack_with_client(
        &mock,
        StackInstallOptions {
            org: "acme",
            stack: "engineering-default",
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();

    let pulls_before_prune = mock.pull_count();
    mock.remove_stack_item("acme", "engineering-default", "api-review-checklist")
        .unwrap();
    let outcome = update_stack_with_client(
        &mock,
        StackUpdateOptions {
            json: false,
            stack: "engineering-default",
            target: InstallTarget::Local,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: false,
            force: false,
            prune: true,
            quiet: true,
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();

    assert!(outcome.updated);
    assert_eq!(
        mock.pull_count(),
        pulls_before_prune,
        "prune-only stack update should not download archives"
    );
    assert_eq!(outcome.pruned.len(), 1);
    assert!(!target_root.join("api-review-checklist").exists());
    let stack_json = stack_receipt(&target_root, "acme", "engineering-default");
    assert_eq!(stack_json["items"].as_array().unwrap().len(), 1);

    let unsafe_root = scratch.join("unsafe-target");
    install_stack_with_client(
        &mock,
        StackInstallOptions {
            org: "acme",
            stack: "engineering-default",
            dest_root: &unsafe_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();
    mock.upsert_stack_item(
        "acme",
        "engineering-default",
        "api-review-checklist",
        VersionPolicy::Current,
        None,
    )
    .unwrap();
    install_stack_with_client(
        &mock,
        StackInstallOptions {
            org: "acme",
            stack: "engineering-default",
            dest_root: &unsafe_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();
    mock.remove_stack_item("acme", "engineering-default", "api-review-checklist")
        .unwrap();
    let child_receipt_path = unsafe_root.join("api-review-checklist/.agentstack-install.json");
    let mut child_receipt: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&child_receipt_path).unwrap()).unwrap();
    child_receipt
        .as_object_mut()
        .unwrap()
        .remove("installed_via");
    child_receipt
        .as_object_mut()
        .unwrap()
        .remove("installed_via_stacks");
    fs::write(
        &child_receipt_path,
        serde_json::to_string_pretty(&child_receipt).unwrap(),
    )
    .unwrap();

    let err = update_stack_with_client(
        &mock,
        StackUpdateOptions {
            json: false,
            stack: "engineering-default",
            target: InstallTarget::Local,
            target_root: &unsafe_root,
            registry_url: Some("mock://registry"),
            check: false,
            force: false,
            prune: true,
            quiet: true,
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("independent install"), "got: {msg}");
    assert!(unsafe_root.join("api-review-checklist/SKILL.md").is_file());
}

#[test]
fn stack_update_non_prune_detaches_dropped_member_to_standalone() {
    let scratch = unique_dir("stack-update-detach");
    let src_incident = scratch.join("src/incident-runbook");
    let src_api = scratch.join("src/api-review-checklist");
    make_skill(
        &src_incident,
        "incident-runbook",
        "Use when handling incidents",
    );
    make_skill(&src_api, "api-review-checklist", "Use when reviewing APIs");

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &src_incident, "incident-runbook");
    publish_approved_skill(&mock, &src_api, "api-review-checklist");
    mock.create_stack(
        "acme",
        "engineering-default",
        "Engineering Default",
        "",
        Visibility::Org,
        None,
    )
    .unwrap();
    for skill in ["incident-runbook", "api-review-checklist"] {
        mock.upsert_stack_item(
            "acme",
            "engineering-default",
            skill,
            VersionPolicy::Current,
            None,
        )
        .unwrap();
    }

    let target_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    install_stack_with_client(
        &mock,
        StackInstallOptions {
            org: "acme",
            stack: "engineering-default",
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();

    mock.remove_stack_item("acme", "engineering-default", "api-review-checklist")
        .unwrap();
    let outcome = update_stack_with_client(
        &mock,
        StackUpdateOptions {
            json: false,
            stack: "engineering-default",
            target: InstallTarget::Local,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: false,
            // force triggers a reinstall so the stack receipt is rewritten to
            // exclude the dropped member; this is the path where the old
            // `merge_removed_stack_receipt_items` re-added it (the wedge).
            force: true,
            prune: false,
            quiet: true,
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();

    assert!(outcome.updated);
    assert_eq!(
        outcome
            .detached
            .iter()
            .map(|item| item.skill.as_str())
            .collect::<Vec<_>>(),
        vec!["api-review-checklist"]
    );
    assert!(outcome.pruned.is_empty());

    // Files stay on disk (not pruned).
    assert!(target_root.join("api-review-checklist/SKILL.md").is_file());

    // The stack receipt no longer lists the dropped member.
    let stack_json = stack_receipt(&target_root, "acme", "engineering-default");
    let stack_items = stack_json["items"].as_array().unwrap();
    assert_eq!(stack_items.len(), 1);
    assert!(
        stack_items
            .iter()
            .all(|item| item["skill"] != "api-review-checklist"),
        "dropped member must not be re-inserted into the stack receipt"
    );

    // The dropped child receipt is detached: standalone, no stack referrers.
    // This is exactly what `install why` / `skill uninstall` consume:
    // empty `installed_via_stacks` => safe to remove, not "required by" the stack.
    let child = receipt(&target_root.join("api-review-checklist"));
    // The empty vec / None are skipped during serialization, so the keys are
    // absent. Either way: no stack referrers remain.
    let refs = child["installed_via_stacks"].as_array();
    assert!(
        refs.map(|r| r.is_empty()).unwrap_or(true),
        "detached member must not list the stack in installed_via_stacks: {child}"
    );
    assert!(
        child["installed_via"].is_null(),
        "detached member with no remaining referrers should have no installed_via: {child}"
    );

    // The kept member is unaffected.
    let kept = receipt(&target_root.join("incident-runbook"));
    assert_eq!(
        kept["installed_via"]["stack"].as_str(),
        Some("engineering-default")
    );
}

#[test]
fn stack_update_non_prune_removal_only_detaches_without_force() {
    // The removal-only path: dropping a member with no other change and no
    // --force/--prune must still detach it, not leave it wedged.
    let scratch = unique_dir("stack-update-detach-noforce");
    let src_incident = scratch.join("src/incident-runbook");
    let src_api = scratch.join("src/api-review-checklist");
    make_skill(
        &src_incident,
        "incident-runbook",
        "Use when handling incidents",
    );
    make_skill(&src_api, "api-review-checklist", "Use when reviewing APIs");

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &src_incident, "incident-runbook");
    publish_approved_skill(&mock, &src_api, "api-review-checklist");
    mock.create_stack(
        "acme",
        "engineering-default",
        "Engineering Default",
        "",
        Visibility::Org,
        None,
    )
    .unwrap();
    for skill in ["incident-runbook", "api-review-checklist"] {
        mock.upsert_stack_item(
            "acme",
            "engineering-default",
            skill,
            VersionPolicy::Current,
            None,
        )
        .unwrap();
    }

    let target_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    install_stack_with_client(
        &mock,
        StackInstallOptions {
            org: "acme",
            stack: "engineering-default",
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();

    mock.remove_stack_item("acme", "engineering-default", "api-review-checklist")
        .unwrap();
    let outcome = update_stack_with_client(
        &mock,
        StackUpdateOptions {
            json: false,
            stack: "engineering-default",
            target: InstallTarget::Local,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: false,
            // No force: a pure removal must still detach via the removal-only
            // fallback (the path that previously left the member wedged).
            force: false,
            prune: false,
            quiet: true,
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();

    assert!(outcome.updated);
    assert_eq!(
        outcome
            .detached
            .iter()
            .map(|item| item.skill.as_str())
            .collect::<Vec<_>>(),
        vec!["api-review-checklist"]
    );
    assert!(outcome.pruned.is_empty());

    // Files stay on disk.
    assert!(target_root.join("api-review-checklist/SKILL.md").is_file());

    // The stack receipt drops the member.
    let stack_json = stack_receipt(&target_root, "acme", "engineering-default");
    let stack_items = stack_json["items"].as_array().unwrap();
    assert_eq!(stack_items.len(), 1);
    assert!(
        stack_items
            .iter()
            .all(|item| item["skill"] != "api-review-checklist"),
    );

    // The dropped child receipt is detached (no stack referrers => removable).
    let child = receipt(&target_root.join("api-review-checklist"));
    let refs = child["installed_via_stacks"].as_array();
    assert!(
        refs.map(|r| r.is_empty()).unwrap_or(true),
        "removal-only detach must clear installed_via_stacks: {child}"
    );
    assert!(child["installed_via"].is_null());

    // The kept member is unaffected.
    let kept = receipt(&target_root.join("incident-runbook"));
    assert_eq!(
        kept["installed_via"]["stack"].as_str(),
        Some("engineering-default")
    );
}

#[test]
fn stack_update_non_prune_drop_keeps_member_owned_by_other_stack() {
    let scratch = unique_dir("stack-update-detach-shared");
    let shared = scratch.join("src/shared-skill");
    make_skill(&shared, "shared-skill", "Use when shared");

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &shared, "shared-skill");
    for stack in ["stack-a", "stack-b"] {
        mock.create_stack("acme", stack, stack, "", Visibility::Org, None)
            .unwrap();
        mock.upsert_stack_item("acme", stack, "shared-skill", VersionPolicy::Current, None)
            .unwrap();
    }

    let target_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    for stack in ["stack-a", "stack-b"] {
        install_stack_with_client(
            &mock,
            StackInstallOptions {
                org: "acme",
                stack,
                dest_root: &target_root,
                target: "local",
                force: false,
                registry_url: Some("mock://registry"),
                installed_by: Some("octocat".to_string()),
                cache_root: Some(&cache_root),
            },
        )
        .unwrap();
    }

    // Drop the shared member from stack-a only.
    mock.remove_stack_item("acme", "stack-a", "shared-skill")
        .unwrap();
    let outcome = update_stack_with_client(
        &mock,
        StackUpdateOptions {
            json: false,
            stack: "stack-a",
            target: InstallTarget::Local,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: false,
            force: true,
            prune: false,
            quiet: true,
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();

    assert!(outcome.updated);
    assert_eq!(
        outcome
            .detached
            .iter()
            .map(|item| item.skill.as_str())
            .collect::<Vec<_>>(),
        vec!["shared-skill"]
    );

    assert!(target_root.join("shared-skill/SKILL.md").is_file());

    // Still owned by stack-b only; stack-a removed from the referrers.
    let child = receipt(&target_root.join("shared-skill"));
    let refs = child["installed_via_stacks"].as_array().unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0]["stack"].as_str(), Some("stack-b"));
    assert_eq!(child["installed_via"]["stack"].as_str(), Some("stack-b"));
}

#[test]
fn stack_update_prune_still_deletes_dropped_member() {
    let scratch = unique_dir("stack-update-prune-deletes");
    let src_incident = scratch.join("src/incident-runbook");
    let src_api = scratch.join("src/api-review-checklist");
    make_skill(
        &src_incident,
        "incident-runbook",
        "Use when handling incidents",
    );
    make_skill(&src_api, "api-review-checklist", "Use when reviewing APIs");

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &src_incident, "incident-runbook");
    publish_approved_skill(&mock, &src_api, "api-review-checklist");
    mock.create_stack(
        "acme",
        "engineering-default",
        "Engineering Default",
        "",
        Visibility::Org,
        None,
    )
    .unwrap();
    for skill in ["incident-runbook", "api-review-checklist"] {
        mock.upsert_stack_item(
            "acme",
            "engineering-default",
            skill,
            VersionPolicy::Current,
            None,
        )
        .unwrap();
    }

    let target_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    install_stack_with_client(
        &mock,
        StackInstallOptions {
            org: "acme",
            stack: "engineering-default",
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();

    mock.remove_stack_item("acme", "engineering-default", "api-review-checklist")
        .unwrap();
    let outcome = update_stack_with_client(
        &mock,
        StackUpdateOptions {
            json: false,
            stack: "engineering-default",
            target: InstallTarget::Local,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: false,
            force: false,
            prune: true,
            quiet: true,
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();

    assert!(outcome.updated);
    assert_eq!(outcome.pruned.len(), 1);
    assert!(outcome.detached.is_empty());
    assert!(!target_root.join("api-review-checklist").exists());
    let stack_json = stack_receipt(&target_root, "acme", "engineering-default");
    assert_eq!(stack_json["items"].as_array().unwrap().len(), 1);
}

#[test]
fn stack_update_current_child_version_updates_receipts() {
    let scratch = unique_dir("stack-update-current-version");
    let src = scratch.join("src/incident-runbook");
    make_skill(&src, "incident-runbook", "Use when handling incidents");

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &src, "incident-runbook");
    mock.create_stack(
        "acme",
        "engineering-default",
        "Engineering Default",
        "",
        Visibility::Org,
        None,
    )
    .unwrap();
    mock.upsert_stack_item(
        "acme",
        "engineering-default",
        "incident-runbook",
        VersionPolicy::Current,
        None,
    )
    .unwrap();

    let target_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    install_stack_with_client(
        &mock,
        StackInstallOptions {
            org: "acme",
            stack: "engineering-default",
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();

    publish_approved_skill(&mock, &src, "incident-runbook");
    mock.approve(&"acme/incident-runbook".parse().unwrap(), "2")
        .unwrap();
    let outcome = update_stack_with_client(
        &mock,
        StackUpdateOptions {
            json: false,
            stack: "engineering-default",
            target: InstallTarget::Local,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: false,
            force: false,
            prune: false,
            quiet: true,
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();

    assert!(outcome.updated);
    assert_eq!(outcome.changed.len(), 1);
    assert_eq!(outcome.changed[0].installed_version, "1");
    assert_eq!(outcome.changed[0].resolved_version, "2");
    let child_receipt = receipt(&target_root.join("incident-runbook"));
    assert_eq!(child_receipt["version"], "2");
    let stack_json = stack_receipt(&target_root, "acme", "engineering-default");
    assert_eq!(stack_json["items"][0]["version"].as_str(), Some("2"));
    assert_eq!(
        child_receipt["installed_via"]["manifest_hash"],
        format!(
            "sha256:{}",
            stack_json["manifest_hash"]["hex"].as_str().unwrap()
        )
    );
}

#[test]
fn update_refuses_stack_owned_child_skill_directly() {
    let scratch = unique_dir("stack-child-direct-update");
    let src = scratch.join("src/incident-runbook");
    make_skill(&src, "incident-runbook", "Use when handling incidents");

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &src, "incident-runbook");
    mock.create_stack(
        "acme",
        "engineering-default",
        "Engineering Default",
        "",
        Visibility::Org,
        None,
    )
    .unwrap();
    mock.upsert_stack_item(
        "acme",
        "engineering-default",
        "incident-runbook",
        VersionPolicy::Current,
        None,
    )
    .unwrap();

    let target_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    install_stack_with_client(
        &mock,
        StackInstallOptions {
            org: "acme",
            stack: "engineering-default",
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();

    publish_approved_skill(&mock, &src, "incident-runbook");
    mock.approve(&"acme/incident-runbook".parse().unwrap(), "2")
        .unwrap();

    let err = update_with_client(
        &mock,
        UpdateOptions {
            json: false,
            skill_name: "incident-runbook",
            target: InstallTarget::Local,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: false,
            force: false,
            quiet: true,
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("stack-owned child skill"), "got: {msg}");
    assert!(
        msg.contains("agentstack stack update acme/engineering-default --target local"),
        "got: {msg}"
    );
    assert_eq!(
        receipt(&target_root.join("incident-runbook"))["version"],
        "1"
    );
}

#[test]
fn stack_update_pinned_child_remains_pinned_when_current_changes() {
    let scratch = unique_dir("stack-update-pinned");
    let src = scratch.join("src/rust-cli-debugging");
    make_skill(&src, "rust-cli-debugging", "Use when debugging Rust CLIs");

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &src, "rust-cli-debugging");
    mock.create_stack(
        "acme",
        "engineering-default",
        "Engineering Default",
        "",
        Visibility::Org,
        None,
    )
    .unwrap();
    mock.upsert_stack_item(
        "acme",
        "engineering-default",
        "rust-cli-debugging",
        VersionPolicy::Pinned,
        Some("1"),
    )
    .unwrap();

    let target_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    install_stack_with_client(
        &mock,
        StackInstallOptions {
            org: "acme",
            stack: "engineering-default",
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();

    publish_approved_skill(&mock, &src, "rust-cli-debugging");
    mock.approve(&"acme/rust-cli-debugging".parse().unwrap(), "2")
        .unwrap();
    let outcome = update_stack_with_client(
        &mock,
        StackUpdateOptions {
            json: false,
            stack: "engineering-default",
            target: InstallTarget::Local,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: false,
            force: false,
            prune: false,
            quiet: true,
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();

    assert!(!outcome.updated);
    assert!(outcome.changed.is_empty());
    assert_eq!(
        receipt(&target_root.join("rust-cli-debugging"))["version"],
        "1"
    );
}

#[test]
fn stack_update_lost_access_to_stack_fails_closed() {
    let scratch = unique_dir("stack-update-lost-access");
    let src = scratch.join("src/incident-runbook");
    make_skill(&src, "incident-runbook", "Use when handling incidents");

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &src, "incident-runbook");
    mock.create_stack(
        "acme",
        "engineering-default",
        "Engineering Default",
        "",
        Visibility::Org,
        None,
    )
    .unwrap();
    mock.upsert_stack_item(
        "acme",
        "engineering-default",
        "incident-runbook",
        VersionPolicy::Current,
        None,
    )
    .unwrap();

    let target_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    install_stack_with_client(
        &mock,
        StackInstallOptions {
            org: "acme",
            stack: "engineering-default",
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap();

    let locked_out = MockRegistryClient::new();
    let err = update_stack_with_client(
        &locked_out,
        StackUpdateOptions {
            json: false,
            stack: "engineering-default",
            target: InstallTarget::Local,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: true,
            force: false,
            prune: false,
            quiet: true,
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
        },
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("resolve stack acme/engineering-default failed"));
    assert!(msg.contains("no such stack"), "got: {msg}");
    assert!(target_root.join("incident-runbook/SKILL.md").is_file());
}

#[test]
fn remote_install_unversioned_fails_without_current_approved_version() {
    let scratch = unique_dir("remote-install-no-current");
    let source = scratch.join("candidate-only");
    make_skill(&source, "candidate-only", "Use when installing candidates");

    let mock = MockRegistryClient::new();
    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();

    let dest_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    let r: SkillRef = "acme/candidate-only".parse().unwrap();
    let err = install_remote_with_client(
        &mock,
        RemoteInstallOptions {
            skill_ref: &r,
            dest_root: &dest_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: None,
            cache_root: Some(&cache_root),
            allow_yanked: false,
        },
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("no approved/current version"), "got: {msg}");
    assert!(!dest_root.exists() || fs::read_dir(&dest_root).unwrap().next().is_none());
}

#[test]
fn remote_install_allow_yanked_requires_pinned_ref() {
    let scratch = unique_dir("remote-install-allow-yanked-pin");
    let mock = MockRegistryClient::new();
    let dest_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    let r: SkillRef = "acme/yanked-install".parse().unwrap();
    let err = install_remote_with_client(
        &mock,
        RemoteInstallOptions {
            skill_ref: &r,
            dest_root: &dest_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: None,
            cache_root: Some(&cache_root),
            allow_yanked: true,
        },
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("--allow-yanked requires an explicit pinned ref"),
        "got: {msg}"
    );
    assert!(
        !dest_root.exists(),
        "preflight error must not touch the install destination"
    );
    assert!(
        !cache_root.exists(),
        "preflight error must not create cache staging"
    );
}

#[test]
fn remote_install_version_ref_installs_that_version() {
    let scratch = unique_dir("remote-install-version");
    let source_v1 = scratch.join("v1/versioned");
    let source_v2 = scratch.join("v2/versioned");
    make_skill(&source_v1, "versioned", "Use when installing v1");
    make_skill(&source_v2, "versioned", "Use when installing v2");

    let mock = MockRegistryClient::new();
    for source in [&source_v1, &source_v2] {
        push_with_client(
            Some(&mock),
            None,
            PushOptions {
                source,
                org: "acme",
                visibility: Visibility::Org,
                team: None,
                platforms: vec![],
                dry_run: false,
            },
        )
        .unwrap();
    }

    let dest_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    let r: SkillRef = "acme/versioned@1".parse().unwrap();
    install_remote_with_client(
        &mock,
        RemoteInstallOptions {
            skill_ref: &r,
            dest_root: &dest_root,
            target: "local",
            force: false,
            registry_url: Some("https://registry.example.com"),
            installed_by: None,
            cache_root: Some(&cache_root),
            allow_yanked: false,
        },
    )
    .unwrap();

    let installed = dest_root.join("versioned");
    let body = fs::read_to_string(installed.join("SKILL.md")).unwrap();
    assert!(body.contains("Use when installing v1"));
    let receipt = receipt(&installed);
    assert_eq!(receipt["version"].as_str(), Some("1"));
}

#[test]
fn remote_install_rejects_bad_hash_before_writing() {
    let scratch = unique_dir("remote-install-bad-hash");
    let mock = MockRegistryClient::new();
    mock.seed(
        SkillMetadata {
            name: "bad-hash".to_string(),
            description: "Use when bad hash".to_string(),
            org: "acme".to_string(),
            owner_email: None,
            team: None,
            visibility: Visibility::Org,
            version: "1".to_string(),
            hash: PackageHash::sha256_of(b"different bytes"),
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
        },
        b"actual bytes".to_vec(),
    );

    let dest_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    let r: SkillRef = "acme/bad-hash@1".parse().unwrap();
    let err = install_remote_with_client(
        &mock,
        RemoteInstallOptions {
            skill_ref: &r,
            dest_root: &dest_root,
            target: "local",
            force: false,
            registry_url: Some("https://registry.example.com"),
            installed_by: None,
            cache_root: Some(&cache_root),
            allow_yanked: false,
        },
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("hash mismatch"));
    assert!(!dest_root.exists() || fs::read_dir(&dest_root).unwrap().next().is_none());
}

#[test]
fn remote_install_rejects_invalid_archive_before_installing() {
    let scratch = unique_dir("remote-install-invalid");
    let mock = MockRegistryClient::new();
    let archive = b"not a gzip archive".to_vec();
    mock.seed(
        SkillMetadata {
            name: "invalid-archive".to_string(),
            description: "Use when invalid archive".to_string(),
            org: "acme".to_string(),
            owner_email: None,
            team: None,
            visibility: Visibility::Org,
            version: "1".to_string(),
            hash: PackageHash::sha256_of(&archive),
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
        },
        archive,
    );

    let dest_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    let r: SkillRef = "acme/invalid-archive@1".parse().unwrap();
    let err = install_remote_with_client(
        &mock,
        RemoteInstallOptions {
            skill_ref: &r,
            dest_root: &dest_root,
            target: "local",
            force: false,
            registry_url: Some("https://registry.example.com"),
            installed_by: None,
            cache_root: Some(&cache_root),
            allow_yanked: false,
        },
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("failed to unpack"));
    assert!(!dest_root.exists() || fs::read_dir(&dest_root).unwrap().next().is_none());
}

#[test]
fn remote_install_rejects_metadata_ref_mismatch_before_staging() {
    let scratch = unique_dir("remote-install-ref-mismatch");
    let source = scratch.join("alpha");
    make_skill(&source, "alpha", "Use when alpha");
    let built = build_skill_package(&source).unwrap();

    let client = StaticPullClient {
        response: PullResponse {
            metadata: SkillMetadata {
                name: "other".to_string(),
                description: "Use when alpha".to_string(),
                org: "acme".to_string(),
                owner_email: None,
                team: None,
                visibility: Visibility::Org,
                version: "1".to_string(),
                hash: built.hash,
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
            },
            archive: built.bytes,
        },
    };

    let dest_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    let r: SkillRef = "acme/alpha@1".parse().unwrap();
    let err = install_remote_with_client(
        &client,
        RemoteInstallOptions {
            skill_ref: &r,
            dest_root: &dest_root,
            target: "local",
            force: false,
            registry_url: Some("https://registry.example.com"),
            installed_by: None,
            cache_root: Some(&cache_root),
            allow_yanked: false,
        },
    )
    .unwrap_err();

    let msg = format!("{err:#}");
    assert!(
        msg.contains("while `acme/alpha@1` was requested"),
        "got: {msg}"
    );
    assert!(!dest_root.exists() || fs::read_dir(&dest_root).unwrap().next().is_none());
    assert!(!cache_root.join("staging").exists());
    assert!(!cache_root.join("skills").exists());
}

#[test]
fn remote_install_rejects_archive_metadata_mismatch_before_installing() {
    let scratch = unique_dir("remote-install-archive-mismatch");
    let source = scratch.join("beta");
    make_skill(&source, "beta", "Use when beta");
    let built = build_skill_package(&source).unwrap();

    let client = StaticPullClient {
        response: PullResponse {
            metadata: SkillMetadata {
                name: "alpha".to_string(),
                description: "Use when alpha".to_string(),
                org: "acme".to_string(),
                owner_email: None,
                team: None,
                visibility: Visibility::Org,
                version: "1".to_string(),
                hash: built.hash,
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
            },
            archive: built.bytes,
        },
    };

    let dest_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    let r: SkillRef = "acme/alpha@1".parse().unwrap();
    let err = install_remote_with_client(
        &client,
        RemoteInstallOptions {
            skill_ref: &r,
            dest_root: &dest_root,
            target: "local",
            force: false,
            registry_url: Some("https://registry.example.com"),
            installed_by: None,
            cache_root: Some(&cache_root),
            allow_yanked: false,
        },
    )
    .unwrap_err();

    let msg = format!("{err:#}");
    assert!(
        msg.contains("metadata name `alpha` does not match archive SKILL.md name `beta`"),
        "got: {msg}"
    );
    assert!(!dest_root.exists() || fs::read_dir(&dest_root).unwrap().next().is_none());
    assert!(!cache_root.join("skills").exists());
}

#[test]
fn remote_install_force_preserves_staged_safety() {
    let scratch = unique_dir("remote-install-force");
    let source = scratch.join("forceable");
    make_skill(&source, "forceable", "Use when force installing");

    let mock = MockRegistryClient::new();
    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();

    let dest_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    let r: SkillRef = "acme/forceable@1".parse().unwrap();
    install_remote_with_client(
        &mock,
        RemoteInstallOptions {
            skill_ref: &r,
            dest_root: &dest_root,
            target: "local",
            force: false,
            registry_url: Some("https://registry.example.com"),
            installed_by: None,
            cache_root: Some(&cache_root),
            allow_yanked: false,
        },
    )
    .unwrap();

    let stale = dest_root.join("forceable/stale.txt");
    fs::write(&stale, "old data").unwrap();
    // Strip the install receipt so the next install treats the dir as foreign
    // (no AgentStack provenance), exercising the no-force refusal path while
    // proving the failed install preserves what's on disk.
    fs::remove_file(dest_root.join("forceable/.agentstack-install.json")).unwrap();
    let err = install_remote_with_client(
        &mock,
        RemoteInstallOptions {
            skill_ref: &r,
            dest_root: &dest_root,
            target: "local",
            force: false,
            registry_url: Some("https://registry.example.com"),
            installed_by: None,
            cache_root: Some(&cache_root),
            allow_yanked: false,
        },
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("refusing to overwrite"));
    assert!(
        stale.is_file(),
        "failed install must preserve existing target"
    );

    install_remote_with_client(
        &mock,
        RemoteInstallOptions {
            skill_ref: &r,
            dest_root: &dest_root,
            target: "local",
            force: true,
            registry_url: Some("https://registry.example.com"),
            installed_by: None,
            cache_root: Some(&cache_root),
            allow_yanked: false,
        },
    )
    .unwrap();
    assert!(!stale.exists());
    assert!(
        dest_root
            .join("forceable/.agentstack-install.json")
            .is_file()
    );
}

#[test]
fn search_renders_useful_output() {
    let mock = MockRegistryClient::new();
    let scratch = unique_dir("search");
    let s1 = scratch.join("code-review");
    let s2 = scratch.join("format-md");
    make_skill(&s1, "code-review", "Use when reviewing pull requests");
    make_skill(&s2, "format-md", "Use to format markdown documents");

    for (path, name) in [(&s1, "code-review"), (&s2, "format-md")] {
        push_with_client(
            Some(&mock),
            None,
            PushOptions {
                source: path,
                org: "acme",
                visibility: Visibility::Org,
                team: None,
                platforms: vec![],
                dry_run: false,
            },
        )
        .unwrap_or_else(|e| panic!("push {name} failed: {e:#}"));
    }

    // Verify the underlying client returns sensible results — text rendering
    // is exercised through the binary in tests/registry_cli.rs.
    let hits = mock.search("review").unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].name, "code-review");

    let all = mock.search("").unwrap();
    assert_eq!(all.len(), 2);
}

#[test]
fn search_workflow_emits_stable_json() {
    // Capture stdout produced by search_with_client when --json is on. We
    // avoid going through the binary so the assertion can compare structured
    // JSON, not free text.
    let mock = MockRegistryClient::new();
    let scratch = unique_dir("search-json");
    let s = scratch.join("alpha");
    make_skill(&s, "alpha", "Use when alpha");
    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &s,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();

    // We can't easily capture stdout from a function that prints directly,
    // but we can assert that the underlying client returns a deterministic
    // shape that serializes cleanly through serde_json. The CLI integration
    // test verifies the full --json round-trip end-to-end.
    let results = mock.search("alpha").unwrap();
    let json = serde_json::to_string(&results).unwrap();
    assert!(json.contains("\"acme\""));
    assert!(json.contains("\"alpha\""));
    assert!(json.contains("\"1\""));

    // Smoke: workflow runs without error.
    search_with_client(&mock, None, "alpha", &SearchFilters::default(), true, false).unwrap();
}

#[test]
fn search_filters_by_org_platform_and_visibility() {
    let mock = MockRegistryClient::new();
    let scratch = unique_dir("search-filters");
    let s1 = scratch.join("review");
    let s2 = scratch.join("format");
    make_skill(&s1, "review", "Use when reviewing code");
    make_skill(&s2, "format", "Use when formatting code");

    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &s1,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec!["codex".to_string()],
            dry_run: false,
        },
    )
    .unwrap();
    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &s2,
            org: "acme",
            visibility: Visibility::Private,
            team: None,
            platforms: vec!["claude-code".to_string()],
            dry_run: false,
        },
    )
    .unwrap();

    let filters = SearchFilters {
        org: Some("acme".to_string()),
        team: None,
        platforms: vec!["codex".to_string()],
        visibility: Some(Visibility::Org),
        owner: None,
        sort: None,
        limit: None,
    };
    let results = mock.search_with_filters("", &filters).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "review");

    let filters = SearchFilters {
        org: Some("acme".to_string()),
        team: None,
        platforms: vec!["codex".to_string(), "claude-code".to_string()],
        visibility: None,
        owner: None,
        sort: None,
        limit: None,
    };
    let results = mock.search_with_filters("", &filters).unwrap();
    assert_eq!(results.len(), 2);
}

#[test]
fn list_remote_filters_by_org() {
    let mock = MockRegistryClient::new();
    let scratch = unique_dir("list-remote");
    let s1 = scratch.join("a1");
    let s2 = scratch.join("b1");
    make_skill(&s1, "a1", "Use when a1");
    make_skill(&s2, "b1", "Use when b1");
    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &s1,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();
    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &s2,
            org: "widgets",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();

    // Smoke: workflow runs successfully with org filter and renders JSON.
    list_remote_with_client(
        &mock,
        None,
        &SearchFilters {
            org: Some("acme".to_string()),
            ..SearchFilters::default()
        },
        true,
        false,
    )
    .unwrap();
    list_remote_with_client(&mock, None, &SearchFilters::default(), false, false).unwrap();
}

#[test]
fn large_catalog_list_and_search_respect_limit_and_owner_metadata() {
    let mock = MockRegistryClient::with_user("owner@example.com");
    let scratch = unique_dir("large-catalog-limit");
    for index in 0..105 {
        let name = format!("catalog-skill-{index:03}");
        let source = scratch.join(&name);
        make_skill(&source, &name, "Use when testing a large catalog");
        push_with_client(
            Some(&mock),
            None,
            PushOptions {
                source: &source,
                org: "acme",
                visibility: Visibility::Org,
                team: None,
                platforms: vec!["codex".to_string()],
                dry_run: false,
            },
        )
        .unwrap();
    }

    let filters = SearchFilters {
        org: Some("acme".to_string()),
        platforms: vec!["codex".to_string()],
        owner: Some("owner@example.com".to_string()),
        sort: Some(CatalogSort::Updated),
        limit: Some(25),
        ..SearchFilters::default()
    };
    let listed = mock.list_remote_with_filters(&filters).unwrap();
    assert_eq!(listed.len(), 25);
    assert_eq!(listed[0].owner_email.as_deref(), Some("owner@example.com"));

    let searched = mock.search_with_filters("catalog-skill", &filters).unwrap();
    assert_eq!(searched.len(), 25);
    assert_eq!(
        searched[0].owner_email.as_deref(),
        Some("owner@example.com")
    );
}

#[test]
fn versions_lists_all_published_versions() {
    let mock = MockRegistryClient::new();
    let scratch = unique_dir("versions");
    let source = scratch.join("alpha");
    make_skill(&source, "alpha", "Use when alpha");
    for _ in 0..3 {
        push_with_client(
            Some(&mock),
            None,
            PushOptions {
                source: &source,
                org: "acme",
                visibility: Visibility::Org,
                team: None,
                platforms: vec![],
                dry_run: false,
            },
        )
        .unwrap();
    }
    mock.approve(&"acme/alpha".parse().unwrap(), "2").unwrap();

    let r: SkillRef = "acme/alpha".parse().unwrap();
    let versions = mock.list_versions(&r).unwrap();
    assert_eq!(versions.len(), 3);
    let current = versions.iter().find(|v| v.current == Some(true)).unwrap();
    assert_eq!(current.version, "2");
    assert_eq!(current.status, Some(VersionStatus::Approved));
    assert!(
        versions
            .iter()
            .any(|v| v.version == "3" && v.status == Some(VersionStatus::Candidate))
    );

    // Smoke: workflow runs and emits JSON.
    versions_with_client(&mock, None, &r, true, false).unwrap();
}

#[test]
fn versions_unknown_skill_is_error() {
    let mock = MockRegistryClient::new();
    let r: SkillRef = "acme/missing".parse().unwrap();
    let err = versions_with_client(&mock, None, &r, false, false).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("missing") || msg.contains("no such"));
}

fn push_skill(mock: &MockRegistryClient, scratch: &Path, org: &str, name: &str, uploads: usize) {
    let source = scratch.join(name);
    make_skill(&source, name, &format!("Use when working on {name}"));
    for _ in 0..uploads {
        push_with_client(
            Some(mock),
            None,
            PushOptions {
                source: &source,
                org,
                visibility: Visibility::Org,
                team: None,
                platforms: vec![],
                dry_run: false,
            },
        )
        .unwrap();
    }
}

#[test]
fn candidates_inbox_aggregates_candidates_across_skills() {
    let mock = MockRegistryClient::with_user("owner@example.com");
    let scratch = unique_dir("candidates-aggregate");
    // alpha: v1 approved, v2 candidate. beta: v1 candidate. gamma: v1 approved.
    push_skill(&mock, &scratch, "acme", "alpha", 2);
    mock.approve(&"acme/alpha".parse().unwrap(), "1").unwrap();
    push_skill(&mock, &scratch, "acme", "beta", 1);
    push_skill(&mock, &scratch, "acme", "gamma", 1);
    mock.approve(&"acme/gamma".parse().unwrap(), "1").unwrap();

    let report = collect_candidates(&mock, "acme", 100).unwrap();
    assert_eq!(report.scanned_skills, 3);
    assert!(!report.truncated);
    assert!(report.skipped.is_empty());
    let refs: Vec<_> = report
        .candidates
        .iter()
        .map(|row| format!("{}/{}@{}", row.org, row.skill, row.version))
        .collect();
    assert_eq!(refs, ["acme/alpha@2", "acme/beta@1"]);
    assert_eq!(
        report.candidates[0].approve_command,
        "agentstack skill version approve acme/alpha@2"
    );
    assert_eq!(
        report.candidates[1].owner.as_deref(),
        Some("owner@example.com")
    );

    // Smoke: workflow renders both human and JSON output.
    for json in [false, true] {
        candidates_with_client(
            &mock,
            None,
            &CandidatesOptions {
                org: "acme",
                limit: 100,
                json,
                quiet: false,
                verbose: false,
            },
        )
        .unwrap();
    }
}

#[test]
fn candidates_inbox_empty_when_everything_is_approved() {
    let mock = MockRegistryClient::new();
    let scratch = unique_dir("candidates-empty");
    push_skill(&mock, &scratch, "acme", "alpha", 1);
    mock.approve(&"acme/alpha".parse().unwrap(), "1").unwrap();

    let report = collect_candidates(&mock, "acme", 100).unwrap();
    assert!(report.candidates.is_empty());
    assert_eq!(report.scanned_skills, 1);
    assert!(!report.truncated);

    // Smoke: empty state renders in both modes without error.
    for json in [false, true] {
        candidates_with_client(
            &mock,
            None,
            &CandidatesOptions {
                org: "acme",
                limit: 100,
                json,
                quiet: false,
                verbose: false,
            },
        )
        .unwrap();
    }
}

#[test]
fn candidates_inbox_caps_scanned_skills_and_reports_truncation() {
    let mock = MockRegistryClient::new();
    let scratch = unique_dir("candidates-limit");
    for name in ["alpha", "beta", "gamma"] {
        push_skill(&mock, &scratch, "acme", name, 1);
    }

    let report = collect_candidates(&mock, "acme", 2).unwrap();
    assert_eq!(report.scanned_skills, 2);
    assert!(report.truncated);
    assert_eq!(report.candidates.len(), 2);

    let full = collect_candidates(&mock, "acme", 3).unwrap();
    assert_eq!(full.scanned_skills, 3);
    assert!(!full.truncated);
    assert_eq!(full.candidates.len(), 3);
}

/// Delegates to a [`MockRegistryClient`] but fails `list_versions` for one
/// skill, simulating a per-skill permission error during the inbox scan.
struct FailingVersionsClient {
    inner: MockRegistryClient,
    fail_for: &'static str,
}

impl RegistryClient for FailingVersionsClient {
    fn ping(&self) -> anyhow::Result<agentstack::registry::PingResponse> {
        self.inner.ping()
    }
    fn whoami(&self) -> anyhow::Result<agentstack::registry::WhoamiResponse> {
        self.inner.whoami()
    }
    fn push(&self, request: PushRequest<'_>) -> anyhow::Result<agentstack::registry::PushResponse> {
        self.inner.push(request)
    }
    fn pull_with_options(
        &self,
        skill_ref: &SkillRef,
        options: PullClientOptions,
    ) -> anyhow::Result<PullResponse> {
        self.inner.pull_with_options(skill_ref, options)
    }
    fn approve(&self, skill_ref: &SkillRef, version: &str) -> anyhow::Result<SkillMetadata> {
        self.inner.approve(skill_ref, version)
    }
    fn yank(
        &self,
        skill_ref: &SkillRef,
        version: &str,
        reason: &str,
    ) -> anyhow::Result<SkillMetadata> {
        self.inner.yank(skill_ref, version, reason)
    }
    fn deprecate(
        &self,
        skill_ref: &SkillRef,
        version: &str,
        reason: &str,
    ) -> anyhow::Result<SkillMetadata> {
        self.inner.deprecate(skill_ref, version, reason)
    }
    fn search(&self, query: &str) -> anyhow::Result<Vec<agentstack::registry::SearchResult>> {
        self.inner.search(query)
    }
    fn list_remote(
        &self,
        org: Option<&str>,
    ) -> anyhow::Result<Vec<agentstack::registry::RemoteSkill>> {
        self.inner.list_remote(org)
    }
    fn list_remote_with_filters(
        &self,
        filters: &SearchFilters,
    ) -> anyhow::Result<Vec<agentstack::registry::RemoteSkill>> {
        self.inner.list_remote_with_filters(filters)
    }
    fn list_versions(
        &self,
        skill_ref: &SkillRef,
    ) -> anyhow::Result<Vec<agentstack::registry::VersionInfo>> {
        if skill_ref.name == self.fail_for {
            anyhow::bail!("403 Forbidden: not allowed to list versions");
        }
        self.inner.list_versions(skill_ref)
    }
}

#[test]
fn candidates_inbox_skips_skills_whose_version_list_fails() {
    let mock = MockRegistryClient::new();
    let scratch = unique_dir("candidates-skip");
    push_skill(&mock, &scratch, "acme", "alpha", 1);
    push_skill(&mock, &scratch, "acme", "beta", 1);
    let client = FailingVersionsClient {
        inner: mock,
        fail_for: "alpha",
    };

    let report = collect_candidates(&client, "acme", 100).unwrap();
    assert_eq!(report.scanned_skills, 2);
    assert_eq!(report.candidates.len(), 1);
    assert_eq!(report.candidates[0].skill, "beta");
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].skill_ref, "acme/alpha");
    assert!(report.skipped[0].error.contains("403"));

    // The render path also succeeds despite the per-skill failure.
    candidates_with_client(
        &client,
        None,
        &CandidatesOptions {
            org: "acme",
            limit: 100,
            json: true,
            quiet: false,
            verbose: true,
        },
    )
    .unwrap();
}

#[test]
fn update_installs_latest_registry_version_from_receipt() {
    let mock = MockRegistryClient::with_user("alice@example.com");
    let scratch = unique_dir("update");
    let source = scratch.join("alpha");
    let target_root = scratch.join("target");
    let cache = scratch.join("cache");
    make_skill(&source, "alpha", "Use when alpha");

    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();
    mock.approve(&"acme/alpha".parse().unwrap(), "1").unwrap();

    let initial: SkillRef = "acme/alpha@1".parse().unwrap();
    install_remote_with_client(
        &mock,
        RemoteInstallOptions {
            skill_ref: &initial,
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
            allow_yanked: false,
        },
    )
    .unwrap();
    let initial_receipt = receipt(&target_root.join("alpha"));
    assert_eq!(initial_receipt["version"], "1");
    assert_eq!(initial_receipt["source_ref"], "acme/alpha");

    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();
    mock.approve(&"acme/alpha".parse().unwrap(), "2").unwrap();

    update_with_client(
        &mock,
        UpdateOptions {
            skill_name: "alpha",
            target: InstallTarget::Local,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: false,
            force: false,
            json: true,
            quiet: false,
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
        },
    )
    .unwrap();

    let installed_receipt = receipt(&target_root.join("alpha"));
    assert_eq!(installed_receipt["version"], "2");
    assert_eq!(installed_receipt["source_ref"], "acme/alpha");
}

fn write_overlay(skill_dir: &Path, platform: &str, name: &str, marker: &str) {
    let overlay = skill_dir.join("platform").join(platform);
    fs::create_dir_all(&overlay).unwrap();
    let body = format!(
        "---\nname: {name}\ndescription: Use when working on {name} tasks\n---\n\n# Purpose\n\n{marker}\n\n# When to Use\n\n# Instructions\n\n# Output\n\n# Boundaries\n",
    );
    fs::write(overlay.join("SKILL.md"), body).unwrap();
}

#[test]
fn install_and_update_apply_platform_overlay_for_target() {
    let mock = MockRegistryClient::with_user("alice@example.com");
    let scratch = unique_dir("overlay-update");
    let source = scratch.join("alpha");
    let target_root = scratch.join("target");
    let cache = scratch.join("cache");
    make_skill(&source, "alpha", "Use when working on alpha tasks");
    write_overlay(&source, "claude-code", "alpha", "Claude Code overlay v1.");

    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();
    mock.approve(&"acme/alpha".parse().unwrap(), "1").unwrap();

    let r: SkillRef = "acme/alpha".parse().unwrap();
    let report = install_remote_with_client(
        &mock,
        RemoteInstallOptions {
            skill_ref: &r,
            dest_root: &target_root,
            target: "claude-code",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
            allow_yanked: false,
        },
    )
    .unwrap();

    let overlay = report
        .install
        .overlay
        .as_ref()
        .expect("install into claude-code target should apply the overlay");
    assert_eq!(overlay.platform, "claude-code");
    assert_eq!(overlay.files, 1);
    let installed = target_root.join("alpha");
    let manifest = fs::read_to_string(installed.join("SKILL.md")).unwrap();
    assert!(manifest.contains("Claude Code overlay v1."));
    assert!(
        installed.join("platform/claude-code/SKILL.md").is_file(),
        "the platform directory itself stays in the installed copy"
    );

    // A new approved version must re-apply the overlay on update.
    write_overlay(&source, "claude-code", "alpha", "Claude Code overlay v2.");
    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();
    mock.approve(&"acme/alpha".parse().unwrap(), "2").unwrap();

    update_with_client(
        &mock,
        UpdateOptions {
            skill_name: "alpha",
            target: InstallTarget::ClaudeCode,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: false,
            force: false,
            json: true,
            quiet: false,
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
        },
    )
    .unwrap();

    let installed_receipt = receipt(&installed);
    assert_eq!(installed_receipt["version"], "2");
    let manifest = fs::read_to_string(installed.join("SKILL.md")).unwrap();
    assert!(
        manifest.contains("Claude Code overlay v2."),
        "update should re-apply the platform overlay"
    );
}

#[test]
fn update_uses_current_approved_version_not_newest_candidate() {
    let mock = MockRegistryClient::with_user("alice@example.com");
    let scratch = unique_dir("update-current");
    let source_v1 = scratch.join("v1/alpha");
    let source_v2 = scratch.join("v2/alpha");
    let target_root = scratch.join("target");
    let cache = scratch.join("cache");
    make_skill(&source_v1, "alpha", "Use when alpha v1");
    make_skill(&source_v2, "alpha", "Use when alpha v2");

    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &source_v1,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();
    mock.approve(&"acme/alpha".parse().unwrap(), "1").unwrap();

    let r: SkillRef = "acme/alpha".parse().unwrap();
    install_remote_with_client(
        &mock,
        RemoteInstallOptions {
            skill_ref: &r,
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
            allow_yanked: false,
        },
    )
    .unwrap();

    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &source_v2,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();

    update_with_client(
        &mock,
        UpdateOptions {
            skill_name: "alpha",
            target: InstallTarget::Local,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: true,
            force: false,
            json: true,
            quiet: false,
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
        },
    )
    .unwrap();
    let installed_receipt = receipt(&target_root.join("alpha"));
    assert_eq!(installed_receipt["version"], "1");

    mock.approve(&"acme/alpha".parse().unwrap(), "2").unwrap();
    update_with_client(
        &mock,
        UpdateOptions {
            skill_name: "alpha",
            target: InstallTarget::Local,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: true,
            force: false,
            json: true,
            quiet: false,
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
        },
    )
    .unwrap();
    update_with_client(
        &mock,
        UpdateOptions {
            skill_name: "alpha",
            target: InstallTarget::Local,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: false,
            force: false,
            json: true,
            quiet: false,
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
        },
    )
    .unwrap();

    let installed_receipt = receipt(&target_root.join("alpha"));
    assert_eq!(installed_receipt["version"], "2");
}

#[test]
fn skill_export_inline_version_matches_flag() {
    let mock = MockRegistryClient::new();
    let scratch = unique_dir("export-flag");
    let source = scratch.join("alpha");
    make_skill(&source, "alpha", "Use when alpha");
    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();

    // Both forms (inline @ and explicit ref+version) hit the same target.
    let r: SkillRef = "acme/alpha".parse().unwrap();
    let r = r.with_version("1").unwrap();
    let dest_parent = scratch.join("from-flag");
    let dest = dest_parent.join("alpha");
    registry_export_with_client(
        &mock,
        None,
        ExportOptions {
            json: false,
            skill_ref: &r,
            out: Some(&dest_parent),
            force: false,
            quiet: false,
            dry_run: false,
            allow_yanked: false,
        },
    )
    .unwrap();
    assert!(dest.join("SKILL.md").is_file());
}

#[test]
fn push_request_carries_archive_bytes_to_client() {
    // Defense in depth: prove that the bytes seen by the registry client
    // are the same bytes that hash to metadata.hash.
    struct CapturingClient {
        captured: std::sync::Mutex<Option<(SkillMetadata, Vec<u8>)>>,
    }
    impl RegistryClient for CapturingClient {
        fn ping(&self) -> anyhow::Result<agentstack::registry::PingResponse> {
            unimplemented!()
        }
        fn whoami(&self) -> anyhow::Result<agentstack::registry::WhoamiResponse> {
            unimplemented!()
        }
        fn push(
            &self,
            request: PushRequest<'_>,
        ) -> anyhow::Result<agentstack::registry::PushResponse> {
            *self.captured.lock().unwrap() =
                Some((request.metadata.clone(), request.archive.to_vec()));
            let metadata = request.metadata;
            Ok(agentstack::registry::PushResponse {
                skill_ref: metadata.skill_ref(),
                version: metadata.version.clone(),
                sha256: metadata.hash.hex.clone(),
                visibility: metadata.visibility,
                metadata,
                url: None,
                audit_event_id: None,
            })
        }
        fn pull_with_options(
            &self,
            _: &SkillRef,
            _: PullClientOptions,
        ) -> anyhow::Result<PullResponse> {
            unimplemented!()
        }
        fn approve(&self, _: &SkillRef, _: &str) -> anyhow::Result<SkillMetadata> {
            unimplemented!()
        }
        fn yank(&self, _: &SkillRef, _: &str, _: &str) -> anyhow::Result<SkillMetadata> {
            unimplemented!()
        }
        fn deprecate(&self, _: &SkillRef, _: &str, _: &str) -> anyhow::Result<SkillMetadata> {
            unimplemented!()
        }
        fn search(&self, _: &str) -> anyhow::Result<Vec<agentstack::registry::SearchResult>> {
            unimplemented!()
        }
        fn list_remote(
            &self,
            _: Option<&str>,
        ) -> anyhow::Result<Vec<agentstack::registry::RemoteSkill>> {
            unimplemented!()
        }
        fn list_versions(
            &self,
            _: &SkillRef,
        ) -> anyhow::Result<Vec<agentstack::registry::VersionInfo>> {
            unimplemented!()
        }
    }

    let scratch = unique_dir("push-bytes");
    let source = scratch.join("alpha");
    make_skill(&source, "alpha", "Use when alpha");

    let client = CapturingClient {
        captured: std::sync::Mutex::new(None),
    };
    push_with_client(
        Some(&client),
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Private,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();

    let captured = client.captured.lock().unwrap().clone().unwrap();
    let (metadata, bytes) = captured;
    let recomputed = PackageHash::sha256_of(&bytes);
    assert_eq!(
        metadata.hash, recomputed,
        "metadata hash must match archive bytes"
    );
}

#[test]
fn update_local_install_error_names_install_source_target_and_command() {
    let mock = MockRegistryClient::new();
    let scratch = unique_dir("update-local-err");
    let source = scratch.join("alpha");
    let target_root = scratch.join("target");
    make_skill(&source, "alpha", "Use when alpha");

    let canonical_source = fs::canonicalize(&source).unwrap().display().to_string();
    install_skill(InstallOptions {
        source: &source,
        dest_root: &target_root,
        name_override: None,
        force: false,
        replace_matching: false,
        receipt: Some(InstallReceiptRequest {
            source_type: ReceiptSourceType::Local,
            source_ref: canonical_source.clone(),
            registry_url: None,
            org: None,
            version: None,
            hash: None,
            content_hash: None,
            target: "local".to_string(),
            installed_by: None,
            installed_via: None,
            installed_via_stacks: Vec::new(),
        }),
    })
    .unwrap();

    let err = update_with_client(
        &mock,
        UpdateOptions {
            json: false,
            skill_name: "alpha",
            target: InstallTarget::Local,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: false,
            force: false,
            quiet: false,
            installed_by: None,
            cache_root: None,
        },
    )
    .unwrap_err();
    let msg = format!("{err:#}");

    assert!(msg.contains("`alpha`"), "missing install name in: {msg}");
    assert!(
        msg.contains(&canonical_source),
        "missing local source `{canonical_source}` in: {msg}"
    );
    assert!(
        msg.contains("target: `local`") || msg.contains("--target local"),
        "missing target `local` in: {msg}"
    );
    assert!(
        msg.contains("agentstack skill install <org>/alpha --target local --force"),
        "missing next-command template in: {msg}"
    );
}

#[test]
fn update_refuses_drifted_destination_without_force_and_repairs_with_force() {
    let mock = MockRegistryClient::with_user("alice@example.com");
    let scratch = unique_dir("update-drift");
    let source = scratch.join("alpha");
    let target_root = scratch.join("target");
    let cache = scratch.join("cache");
    make_skill(&source, "alpha", "Use when alpha");

    for _ in 0..2 {
        push_with_client(
            Some(&mock),
            None,
            PushOptions {
                source: &source,
                org: "acme",
                visibility: Visibility::Org,
                team: None,
                platforms: vec![],
                dry_run: false,
            },
        )
        .unwrap();
    }
    mock.approve(&"acme/alpha".parse().unwrap(), "2").unwrap();

    let initial: SkillRef = "acme/alpha@1".parse().unwrap();
    install_remote_with_client(
        &mock,
        RemoteInstallOptions {
            skill_ref: &initial,
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
            allow_yanked: false,
        },
    )
    .unwrap();

    // Simulate a drifted destination: the on-disk receipt is still readable
    // (so update can derive a skill ref) but its identity no longer matches
    // what an `acme/alpha` install would write — here, the org has been
    // dropped, leaving a Registry receipt with no org.
    let receipt_path = target_root.join("alpha/.agentstack-install.json");
    let mut receipt_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&receipt_path).unwrap()).unwrap();
    receipt_json.as_object_mut().unwrap().remove("org");
    fs::write(
        &receipt_path,
        serde_json::to_string_pretty(&receipt_json).unwrap(),
    )
    .unwrap();

    let err = update_with_client(
        &mock,
        UpdateOptions {
            json: false,
            skill_name: "alpha",
            target: InstallTarget::Local,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: false,
            force: false,
            quiet: true,
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
        },
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("refusing to overwrite"), "got: {msg}");
    assert!(msg.contains("registry skill `acme/alpha`"), "got: {msg}");

    update_with_client(
        &mock,
        UpdateOptions {
            json: false,
            skill_name: "alpha",
            target: InstallTarget::Local,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: false,
            force: true,
            quiet: true,
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
        },
    )
    .unwrap();

    let installed_receipt = receipt(&target_root.join("alpha"));
    assert_eq!(installed_receipt["org"], "acme");
    assert_eq!(installed_receipt["version"], "2");
    assert_eq!(installed_receipt["source_ref"], "acme/alpha");
}

#[test]
fn update_refuses_yanked_current_version() {
    let mock = MockRegistryClient::with_user("alice@example.com");
    let scratch = unique_dir("update-yanked-current");
    let (target_root, cache) = install_two_under_target(&mock, &scratch);
    let skill_ref: SkillRef = "acme/alpha".parse().unwrap();
    mock.yank(&skill_ref, "1", "bad archive").unwrap();

    let err = update_with_client(
        &mock,
        UpdateOptions {
            json: false,
            skill_name: "alpha",
            target: InstallTarget::Local,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: true,
            force: false,
            quiet: true,
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
        },
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("approved/current version `acme/alpha@1` was yanked: bad archive"),
        "got: {msg}"
    );
}

#[test]
fn update_check_surfaces_yanked_installed_version_with_newer_current() {
    let mock = MockRegistryClient::with_user("alice@example.com");
    let scratch = unique_dir("update-yanked-installed");
    let (target_root, cache) = install_two_under_target(&mock, &scratch);

    let src_alpha = scratch.join("src/alpha");
    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &src_alpha,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();
    let skill_ref: SkillRef = "acme/alpha".parse().unwrap();
    mock.approve(&skill_ref, "2").unwrap();
    mock.yank(&skill_ref, "1", "bad archive").unwrap();

    let outcome = update_all_with_client(
        &mock,
        UpdateAllOptions {
            rows: batch_rows(&target_root, &["alpha"]),
            target_filter: Some(InstallTarget::Local),
            registry_url: Some("mock://registry"),
            check: true,
            force: false,
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
        },
    );

    assert_eq!(outcome.update_available_count(), 1);
    match &outcome.results[0].status {
        BatchUpdateRowStatus::UpdateAvailable {
            installed_version,
            latest_version,
            installed_yanked,
            ..
        } => {
            assert_eq!(installed_version.as_deref(), Some("1"));
            assert_eq!(latest_version, "2");
            assert_eq!(installed_yanked.as_deref(), Some("bad archive"));
        }
        other => panic!("expected yanked update availability, got {other:?}"),
    }
}

fn install_two_under_target(
    mock: &MockRegistryClient,
    scratch: &Path,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let target_root = scratch.join("target");
    let cache = scratch.join("cache");
    let src_alpha = scratch.join("src/alpha");
    let src_beta = scratch.join("src/beta");
    make_skill(&src_alpha, "alpha", "Use when alpha");
    make_skill(&src_beta, "beta", "Use when beta");

    for (path, name, version) in [(&src_alpha, "alpha", "1"), (&src_beta, "beta", "1")] {
        push_with_client(
            Some(mock),
            None,
            PushOptions {
                source: path,
                org: "acme",
                visibility: Visibility::Org,
                team: None,
                platforms: vec![],
                dry_run: false,
            },
        )
        .unwrap();
        mock.approve(&format!("acme/{name}").parse().unwrap(), version)
            .unwrap();
        let r: SkillRef = format!("acme/{name}").parse().unwrap();
        install_remote_with_client(
            mock,
            RemoteInstallOptions {
                skill_ref: &r,
                dest_root: &target_root,
                target: "local",
                force: false,
                registry_url: Some("mock://registry"),
                installed_by: Some("alice@example.com".to_string()),
                cache_root: Some(&cache),
                allow_yanked: false,
            },
        )
        .unwrap();
    }
    (target_root, cache)
}

fn batch_rows(target_root: &Path, names: &[&str]) -> Vec<BatchUpdateRow> {
    names
        .iter()
        .map(|n| {
            let installed_path = target_root.join(n);
            BatchUpdateRow {
                target: InstallTarget::Local,
                target_root: target_root.to_path_buf(),
                installed_path: installed_path.clone(),
                receipt_path: receipt_path(&installed_path),
                skill_name: (*n).to_string(),
                receipt: read_receipt_from_dir(&installed_path).unwrap(),
            }
        })
        .collect()
}

#[test]
fn update_all_check_reports_each_receipt_without_writing() {
    let mock = MockRegistryClient::with_user("alice@example.com");
    let scratch = unique_dir("update-all-check");
    let (target_root, cache) = install_two_under_target(&mock, &scratch);

    // Push and approve a newer version of alpha only.
    let src_alpha = scratch.join("src/alpha");
    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &src_alpha,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();
    mock.approve(&"acme/alpha".parse().unwrap(), "2").unwrap();

    let outcome = update_all_with_client(
        &mock,
        UpdateAllOptions {
            rows: batch_rows(&target_root, &["alpha", "beta"]),
            target_filter: Some(InstallTarget::Local),
            registry_url: Some("mock://registry"),
            check: true,
            force: false,
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
        },
    );

    assert_eq!(outcome.results.len(), 2);
    assert_eq!(outcome.update_available_count(), 1);
    assert_eq!(outcome.already_current_count(), 1);
    assert_eq!(outcome.failed_count(), 0);
    assert_eq!(outcome.updated_count(), 0);
    // Files should be untouched.
    assert_eq!(receipt(&target_root.join("alpha"))["version"], "1");
    assert_eq!(receipt(&target_root.join("beta"))["version"], "1");
}

#[test]
fn update_all_check_previews_file_changes_per_receipt() {
    let mock = MockRegistryClient::with_user("alice@example.com");
    let scratch = unique_dir("update-all-check-preview");
    let (target_root, cache) = install_two_under_target(&mock, &scratch);

    // Push a modified alpha: one added file and a changed SKILL.md.
    let src_alpha = scratch.join("src/alpha");
    fs::create_dir_all(src_alpha.join("references")).unwrap();
    fs::write(src_alpha.join("references/new.md"), "new note\n").unwrap();
    let skill_md = fs::read_to_string(src_alpha.join("SKILL.md")).unwrap();
    fs::write(
        src_alpha.join("SKILL.md"),
        format!("{skill_md}\nUpdated guidance.\n"),
    )
    .unwrap();
    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &src_alpha,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();
    mock.approve(&"acme/alpha".parse().unwrap(), "2").unwrap();

    let outcome = update_all_with_client(
        &mock,
        UpdateAllOptions {
            rows: batch_rows(&target_root, &["alpha"]),
            target_filter: Some(InstallTarget::Local),
            registry_url: Some("mock://registry"),
            check: true,
            force: false,
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
        },
    );

    assert_eq!(outcome.update_available_count(), 1);
    match &outcome.results[0].status {
        BatchUpdateRowStatus::UpdateAvailable { changes, .. } => {
            let changes = changes.as_ref().expect("expected file change preview");
            assert_eq!(changes.added, vec!["references/new.md"]);
            assert!(changes.removed.is_empty());
            assert_eq!(changes.changed, vec!["SKILL.md"]);
        }
        other => panic!("expected update availability with changes, got {other:?}"),
    }
    // The preview must not modify the installed copy.
    assert_eq!(receipt(&target_root.join("alpha"))["version"], "1");
    assert!(!target_root.join("alpha/references/new.md").exists());
}

/// Delegates version listing to the mock but fails archive downloads, so
/// `--check` previews must degrade to a plain version delta.
struct FailingPullClient<'a> {
    inner: &'a MockRegistryClient,
}

impl RegistryClient for FailingPullClient<'_> {
    fn ping(&self) -> anyhow::Result<agentstack::registry::PingResponse> {
        self.inner.ping()
    }

    fn whoami(&self) -> anyhow::Result<agentstack::registry::WhoamiResponse> {
        self.inner.whoami()
    }

    fn push(&self, request: PushRequest<'_>) -> anyhow::Result<agentstack::registry::PushResponse> {
        self.inner.push(request)
    }

    fn pull_with_options(
        &self,
        _skill_ref: &SkillRef,
        _options: PullClientOptions,
    ) -> anyhow::Result<PullResponse> {
        anyhow::bail!("archive download unavailable")
    }

    fn approve(&self, skill_ref: &SkillRef, version: &str) -> anyhow::Result<SkillMetadata> {
        self.inner.approve(skill_ref, version)
    }

    fn yank(
        &self,
        skill_ref: &SkillRef,
        version: &str,
        reason: &str,
    ) -> anyhow::Result<SkillMetadata> {
        self.inner.yank(skill_ref, version, reason)
    }

    fn deprecate(
        &self,
        skill_ref: &SkillRef,
        version: &str,
        reason: &str,
    ) -> anyhow::Result<SkillMetadata> {
        self.inner.deprecate(skill_ref, version, reason)
    }

    fn search(&self, query: &str) -> anyhow::Result<Vec<agentstack::registry::SearchResult>> {
        self.inner.search(query)
    }

    fn list_remote(
        &self,
        org: Option<&str>,
    ) -> anyhow::Result<Vec<agentstack::registry::RemoteSkill>> {
        self.inner.list_remote(org)
    }

    fn list_versions(
        &self,
        skill_ref: &SkillRef,
    ) -> anyhow::Result<Vec<agentstack::registry::VersionInfo>> {
        self.inner.list_versions(skill_ref)
    }
}

#[test]
fn update_all_check_preview_degrades_when_download_fails() {
    let mock = MockRegistryClient::with_user("alice@example.com");
    let scratch = unique_dir("update-all-check-degrade");
    let (target_root, cache) = install_two_under_target(&mock, &scratch);

    let src_alpha = scratch.join("src/alpha");
    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &src_alpha,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();
    mock.approve(&"acme/alpha".parse().unwrap(), "2").unwrap();

    let failing = FailingPullClient { inner: &mock };
    let outcome = update_all_with_client(
        &failing,
        UpdateAllOptions {
            rows: batch_rows(&target_root, &["alpha"]),
            target_filter: Some(InstallTarget::Local),
            registry_url: Some("mock://registry"),
            check: true,
            force: false,
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
        },
    );

    // The version delta is still reported; only the preview is missing.
    assert_eq!(outcome.update_available_count(), 1);
    assert_eq!(outcome.failed_count(), 0);
    match &outcome.results[0].status {
        BatchUpdateRowStatus::UpdateAvailable {
            installed_version,
            latest_version,
            changes,
            ..
        } => {
            assert_eq!(installed_version.as_deref(), Some("1"));
            assert_eq!(latest_version, "2");
            assert!(changes.is_none());
        }
        other => panic!("expected degraded update availability, got {other:?}"),
    }
}

#[test]
fn update_all_check_uses_scanned_receipts_without_rereading() {
    let mock = MockRegistryClient::with_user("alice@example.com");
    let scratch = unique_dir("update-all-carried-receipt");
    let (target_root, cache) = install_two_under_target(&mock, &scratch);

    let src_alpha = scratch.join("src/alpha");
    push_with_client(
        Some(&mock),
        None,
        PushOptions {
            source: &src_alpha,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();
    mock.approve(&"acme/alpha".parse().unwrap(), "2").unwrap();

    let rows = batch_rows(&target_root, &["alpha"]);
    fs::remove_file(receipt_path(&target_root.join("alpha"))).unwrap();

    let outcome = update_all_with_client(
        &mock,
        UpdateAllOptions {
            rows,
            target_filter: Some(InstallTarget::Local),
            registry_url: Some("mock://registry"),
            check: true,
            force: false,
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
        },
    );

    assert_eq!(outcome.results.len(), 1);
    assert_eq!(outcome.update_available_count(), 1, "{:?}", outcome.results);
    assert_eq!(outcome.failed_count(), 0, "{:?}", outcome.results);
}

#[test]
fn update_all_rejects_mismatched_scanned_row_path() {
    let mock = MockRegistryClient::with_user("alice@example.com");
    let scratch = unique_dir("update-all-row-mismatch");
    let (target_root, cache) = install_two_under_target(&mock, &scratch);

    let mut rows = batch_rows(&target_root, &["alpha"]);
    rows[0].installed_path = target_root.join("beta");

    let outcome = update_all_with_client(
        &mock,
        UpdateAllOptions {
            rows,
            target_filter: Some(InstallTarget::Local),
            registry_url: Some("mock://registry"),
            check: true,
            force: false,
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
        },
    );

    assert_eq!(outcome.failed_count(), 1, "{:?}", outcome.results);
    match &outcome.results[0].status {
        BatchUpdateRowStatus::Failed { reason } => {
            assert!(
                reason.contains("points at"),
                "unexpected mismatch reason: {reason}"
            );
        }
        other => panic!("expected row mismatch to fail, got {other:?}"),
    }
}

#[test]
fn update_all_updates_each_outdated_receipt_and_continues_on_failure() {
    let mock = MockRegistryClient::with_user("alice@example.com");
    let scratch = unique_dir("update-all-apply");
    let (target_root, cache) = install_two_under_target(&mock, &scratch);

    // Approve newer versions for both alpha and beta.
    for (name, version) in [("alpha", "2"), ("beta", "2")] {
        let src = scratch.join(format!("src/{name}"));
        push_with_client(
            Some(&mock),
            None,
            PushOptions {
                source: &src,
                org: "acme",
                visibility: Visibility::Org,
                team: None,
                platforms: vec![],
                dry_run: false,
            },
        )
        .unwrap();
        mock.approve(&format!("acme/{name}").parse().unwrap(), version)
            .unwrap();
    }

    // Install a third skill from a *local* path so its receipt is non-registry
    // and the batch update should record it as a failure but still process
    // the registry rows.
    let local_src = scratch.join("local-src/gamma");
    make_skill(&local_src, "gamma", "Use when gamma");
    let canonical_local = fs::canonicalize(&local_src).unwrap().display().to_string();
    install_skill(InstallOptions {
        source: &local_src,
        dest_root: &target_root,
        name_override: None,
        force: false,
        replace_matching: false,
        receipt: Some(InstallReceiptRequest {
            source_type: ReceiptSourceType::Local,
            source_ref: canonical_local,
            registry_url: None,
            org: None,
            version: None,
            hash: None,
            content_hash: None,
            target: "local".to_string(),
            installed_by: None,
            installed_via: None,
            installed_via_stacks: Vec::new(),
        }),
    })
    .unwrap();

    let outcome = update_all_with_client(
        &mock,
        UpdateAllOptions {
            rows: batch_rows(&target_root, &["alpha", "beta", "gamma"]),
            target_filter: None,
            registry_url: Some("mock://registry"),
            check: false,
            force: false,
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
        },
    );

    assert_eq!(outcome.updated_count(), 2, "{:?}", outcome.results);
    assert_eq!(outcome.failed_count(), 1);
    let gamma = outcome
        .results
        .iter()
        .find(|r| r.skill_name == "gamma")
        .unwrap();
    match &gamma.status {
        BatchUpdateRowStatus::Failed { reason } => {
            assert!(
                reason.contains("local install receipt"),
                "unexpected gamma reason: {reason}"
            );
        }
        other => panic!("expected gamma to fail, got {other:?}"),
    }
    assert_eq!(receipt(&target_root.join("alpha"))["version"], "2");
    assert_eq!(receipt(&target_root.join("beta"))["version"], "2");
}

#[test]
fn update_up_to_date_drifted_install_does_not_clobber_without_force() {
    let mock = MockRegistryClient::with_user("alice@example.com");
    let scratch = unique_dir("update-drift-noforce");
    let source = scratch.join("alpha");
    let target_root = scratch.join("target");
    let cache = scratch.join("cache");
    make_skill(&source, "alpha", "Use when alpha");
    publish_approved_skill(&mock, &source, "alpha");

    let r: SkillRef = "acme/alpha".parse().unwrap();
    install_remote_with_client(
        &mock,
        RemoteInstallOptions {
            skill_ref: &r,
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
            allow_yanked: false,
        },
    )
    .unwrap();

    // Hand-edit a tracked file under the install to create drift.
    let installed_skill = target_root.join("alpha").join("SKILL.md");
    let mut body = fs::read_to_string(&installed_skill).unwrap();
    body.push_str("\n# Local hand edit\nAdded by a user.\n");
    fs::write(&installed_skill, &body).unwrap();
    let recorded_version = receipt(&target_root.join("alpha"))["version"].clone();

    // Up to date with the registry, but drifted. Without --force this must NOT
    // overwrite the local modifications.
    update_with_client(
        &mock,
        UpdateOptions {
            json: false,
            skill_name: "alpha",
            target: InstallTarget::Local,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: false,
            force: false,
            quiet: true,
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
        },
    )
    .unwrap();

    let after = fs::read_to_string(&installed_skill).unwrap();
    assert_eq!(after, body, "local edit must be preserved without --force");
    assert_eq!(
        receipt(&target_root.join("alpha"))["version"],
        recorded_version,
        "receipt version must be unchanged"
    );

    // --force restores the registry copy and clears drift.
    update_with_client(
        &mock,
        UpdateOptions {
            json: false,
            skill_name: "alpha",
            target: InstallTarget::Local,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: false,
            force: true,
            quiet: true,
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
        },
    )
    .unwrap();

    let restored = fs::read_to_string(&installed_skill).unwrap();
    assert!(
        !restored.contains("Local hand edit"),
        "--force must discard local edits and restore the registry copy"
    );
    // The restored install must re-hash back to the recorded package hash.
    let recorded_hash = receipt(&target_root.join("alpha"))["hash"]
        .as_str()
        .unwrap()
        .to_string();
    let rebuilt = build_skill_package(&target_root.join("alpha")).unwrap();
    assert_eq!(
        format_hash(&rebuilt.hash),
        recorded_hash,
        "restored files must match recorded package hash (no drift)"
    );
}

/// Install acme/alpha@1, hand-edit its SKILL.md to create content drift, then
/// publish + approve version 2. Returns `(target_root, cache, edited SKILL.md
/// path, edited body)`.
fn install_drifted_alpha_with_update_available(
    mock: &MockRegistryClient,
    scratch: &Path,
) -> (PathBuf, PathBuf, PathBuf, String) {
    let source = scratch.join("alpha");
    let target_root = scratch.join("target");
    let cache = scratch.join("cache");
    make_skill(&source, "alpha", "Use when alpha v1");
    publish_approved_skill(mock, &source, "alpha");

    let r: SkillRef = "acme/alpha".parse().unwrap();
    install_remote_with_client(
        mock,
        RemoteInstallOptions {
            skill_ref: &r,
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some("mock://registry"),
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
            allow_yanked: false,
        },
    )
    .unwrap();

    // Drift the install, then publish + approve a newer version.
    let installed_skill = target_root.join("alpha").join("SKILL.md");
    let mut body = fs::read_to_string(&installed_skill).unwrap();
    body.push_str("\n# Local hand edit\nAdded by a user.\n");
    fs::write(&installed_skill, &body).unwrap();

    make_skill(&source, "alpha", "Use when alpha v2 with more detail");
    push_with_client(
        Some(mock),
        None,
        PushOptions {
            source: &source,
            org: "acme",
            visibility: Visibility::Org,
            team: None,
            platforms: vec![],
            dry_run: false,
        },
    )
    .unwrap();
    mock.approve(&"acme/alpha".parse().unwrap(), "2").unwrap();
    (target_root, cache, installed_skill, body)
}

#[test]
fn update_refuses_content_drifted_install_without_force_and_overwrites_with_force() {
    let mock = MockRegistryClient::with_user("alice@example.com");
    let scratch = unique_dir("update-drift-upgrade");
    let (target_root, cache, installed_skill, body) =
        install_drifted_alpha_with_update_available(&mock, &scratch);

    // An update is available AND the install drifted. Without --force the
    // update must refuse and leave the destination untouched.
    let err = update_with_client(
        &mock,
        UpdateOptions {
            json: false,
            skill_name: "alpha",
            target: InstallTarget::Local,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: false,
            force: false,
            quiet: true,
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
        },
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("local modifications"), "got: {msg}");
    assert!(
        msg.contains("agentstack skill diff alpha --target local"),
        "got: {msg}"
    );
    assert!(msg.contains("--force"), "got: {msg}");
    assert_eq!(
        fs::read_to_string(&installed_skill).unwrap(),
        body,
        "refusal must leave the local edit untouched"
    );
    assert_eq!(receipt(&target_root.join("alpha"))["version"], "1");

    // --force opts in to the overwrite.
    update_with_client(
        &mock,
        UpdateOptions {
            json: false,
            skill_name: "alpha",
            target: InstallTarget::Local,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: false,
            force: true,
            quiet: true,
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
        },
    )
    .unwrap();

    assert_eq!(receipt(&target_root.join("alpha"))["version"], "2");
    let after = fs::read_to_string(&installed_skill).unwrap();
    assert!(
        !after.contains("Local hand edit"),
        "--force must overwrite local edits"
    );
}

#[test]
fn update_check_on_content_drifted_install_reports_without_blocking() {
    let mock = MockRegistryClient::with_user("alice@example.com");
    let scratch = unique_dir("update-drift-check");
    let (target_root, cache, installed_skill, body) =
        install_drifted_alpha_with_update_available(&mock, &scratch);

    // --check is read-only: it must succeed on a drifted install and leave it
    // untouched (drift is surfaced via `content_drifted` and the --force next
    // command).
    update_with_client(
        &mock,
        UpdateOptions {
            json: false,
            skill_name: "alpha",
            target: InstallTarget::Local,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: true,
            force: false,
            quiet: true,
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
        },
    )
    .unwrap();

    assert_eq!(fs::read_to_string(&installed_skill).unwrap(), body);
    assert_eq!(receipt(&target_root.join("alpha"))["version"], "1");
}

#[test]
fn update_all_fails_content_drifted_row_and_continues() {
    let mock = MockRegistryClient::with_user("alice@example.com");
    let scratch = unique_dir("update-all-drift");
    let (target_root, cache) = install_two_under_target(&mock, &scratch);

    // Approve newer versions for both alpha and beta.
    for (name, version) in [("alpha", "2"), ("beta", "2")] {
        let src = scratch.join(format!("src/{name}"));
        push_with_client(
            Some(&mock),
            None,
            PushOptions {
                source: &src,
                org: "acme",
                visibility: Visibility::Org,
                team: None,
                platforms: vec![],
                dry_run: false,
            },
        )
        .unwrap();
        mock.approve(&format!("acme/{name}").parse().unwrap(), version)
            .unwrap();
    }

    // Drift alpha only.
    let installed_skill = target_root.join("alpha").join("SKILL.md");
    let mut body = fs::read_to_string(&installed_skill).unwrap();
    body.push_str("\n# Local hand edit\nAdded by a user.\n");
    fs::write(&installed_skill, &body).unwrap();

    let outcome = update_all_with_client(
        &mock,
        UpdateAllOptions {
            rows: batch_rows(&target_root, &["alpha", "beta"]),
            target_filter: None,
            registry_url: Some("mock://registry"),
            check: false,
            force: false,
            installed_by: Some("alice@example.com".to_string()),
            cache_root: Some(&cache),
        },
    );

    assert_eq!(outcome.failed_count(), 1, "{:?}", outcome.results);
    assert_eq!(outcome.updated_count(), 1, "{:?}", outcome.results);
    let alpha = outcome
        .results
        .iter()
        .find(|r| r.skill_name == "alpha")
        .unwrap();
    match &alpha.status {
        BatchUpdateRowStatus::Failed { reason } => {
            assert!(
                reason.contains("local modifications") && reason.contains("--force"),
                "unexpected alpha reason: {reason}"
            );
        }
        other => panic!("expected drifted alpha row to fail, got {other:?}"),
    }
    // The drifted install is untouched; the clean row still updated.
    assert_eq!(fs::read_to_string(&installed_skill).unwrap(), body);
    assert_eq!(receipt(&target_root.join("alpha"))["version"], "1");
    assert_eq!(receipt(&target_root.join("beta"))["version"], "2");
}
