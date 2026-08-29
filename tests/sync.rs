//! Workflow tests for `agentstack sync` driven against an in-memory
//! [`MockRegistryClient`], plus one binary test for the missing-manifest
//! error path.
//!
//! [`MockRegistryClient`]: agentstack::registry::MockRegistryClient

use std::fs;
use std::path::{Path, PathBuf};

use agentstack::commands::{
    PushOptions, RemoteInstallOptions, SyncAction, SyncManifest, SyncOptions,
    install_remote_with_client, load_sync_manifest, push_with_client, sync_with_client,
};
use agentstack::registry::{MockRegistryClient, RegistryClient, VersionPolicy, Visibility};
use agentstack::skill_ref::SkillRef;
use agentstack::targets::InstallTarget;

const REGISTRY_URL: &str = "mock://registry";

fn unique_dir(prefix: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "agentstack-sync-{prefix}-{}-{}",
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

fn push_skill(mock: &MockRegistryClient, source: &Path) {
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
}

fn publish_approved_skill(mock: &MockRegistryClient, source: &Path, name: &str) {
    push_skill(mock, source);
    mock.approve(&format!("acme/{name}").parse().unwrap(), "1")
        .unwrap();
}

fn manifest(scratch: &Path, text: &str) -> SyncManifest {
    let path = scratch.join("agentstack.toml");
    fs::write(&path, text).unwrap();
    load_sync_manifest(&path).unwrap()
}

fn sync_options<'a>(
    manifest: &'a SyncManifest,
    target_roots: &'a [(InstallTarget, PathBuf)],
    cache_root: &'a Path,
    check: bool,
    prune: bool,
) -> SyncOptions<'a> {
    SyncOptions {
        manifest,
        target_roots,
        check,
        prune,
        registry_url: Some(REGISTRY_URL),
        installed_by: Some("octocat".to_string()),
        cache_root: Some(cache_root),
    }
}

#[test]
fn sync_installs_missing_skill_and_stack_then_noops() {
    let scratch = unique_dir("fresh-install");
    let src_review = scratch.join("src/code-review");
    let src_incident = scratch.join("src/incident-runbook");
    make_skill(&src_review, "code-review", "Use when reviewing code");
    make_skill(
        &src_incident,
        "incident-runbook",
        "Use when handling incidents",
    );

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &src_review, "code-review");
    publish_approved_skill(&mock, &src_incident, "incident-runbook");
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
    let manifest = manifest(
        &scratch,
        "[[stacks]]\nref = \"acme/engineering-default\"\ntarget = \"local\"\n\n\
         [[skills]]\nref = \"acme/code-review\"\ntarget = \"local\"\n",
    );
    let roots = vec![(InstallTarget::Local, target_root.clone())];

    let outcome = sync_with_client(
        &mock,
        sync_options(&manifest, &roots, &cache_root, false, false),
    )
    .unwrap();

    assert_eq!(outcome.entries.len(), 2);
    assert!(
        outcome
            .entries
            .iter()
            .all(|entry| entry.action == SyncAction::Installed),
        "expected both entries installed: {outcome:?}"
    );
    assert!(target_root.join("code-review/SKILL.md").is_file());
    assert!(target_root.join("incident-runbook/SKILL.md").is_file());
    assert!(
        target_root
            .join(".agentstack-stacks/acme/engineering-default/.agentstack.json")
            .is_file()
    );

    // Second run is a no-op: everything reports up-to-date.
    let second = sync_with_client(
        &mock,
        sync_options(&manifest, &roots, &cache_root, false, false),
    )
    .unwrap();
    assert!(
        second
            .entries
            .iter()
            .all(|entry| entry.action == SyncAction::UpToDate),
        "expected both entries up-to-date: {second:?}"
    );
}

#[test]
fn sync_check_reports_pending_actions_without_writing() {
    let scratch = unique_dir("check");
    let src = scratch.join("src/code-review");
    make_skill(&src, "code-review", "Use when reviewing code");

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &src, "code-review");

    let target_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    let manifest = manifest(
        &scratch,
        "[[skills]]\nref = \"acme/code-review\"\ntarget = \"local\"\n",
    );
    let roots = vec![(InstallTarget::Local, target_root.clone())];

    let outcome = sync_with_client(
        &mock,
        sync_options(&manifest, &roots, &cache_root, true, false),
    )
    .unwrap();

    assert_eq!(outcome.entries.len(), 1);
    assert_eq!(outcome.entries[0].action, SyncAction::WouldInstall);
    assert_eq!(outcome.pending_count(), 1);
    assert_eq!(mock.pull_count(), 0, "check must not download archives");
    assert!(
        !target_root.exists(),
        "check must not create the install target"
    );
}

#[test]
fn sync_updates_outdated_unpinned_skill() {
    let scratch = unique_dir("update");
    let src_v1 = scratch.join("v1/code-review");
    let src_v2 = scratch.join("v2/code-review");
    make_skill(&src_v1, "code-review", "Use when reviewing code (v1)");
    make_skill(&src_v2, "code-review", "Use when reviewing code (v2)");

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &src_v1, "code-review");

    let target_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    let manifest = manifest(
        &scratch,
        "[[skills]]\nref = \"acme/code-review\"\ntarget = \"local\"\n",
    );
    let roots = vec![(InstallTarget::Local, target_root.clone())];

    sync_with_client(
        &mock,
        sync_options(&manifest, &roots, &cache_root, false, false),
    )
    .unwrap();

    push_skill(&mock, &src_v2);
    mock.approve(&"acme/code-review".parse().unwrap(), "2")
        .unwrap();

    let check = sync_with_client(
        &mock,
        sync_options(&manifest, &roots, &cache_root, true, false),
    )
    .unwrap();
    assert_eq!(check.entries[0].action, SyncAction::WouldUpdate);
    assert_eq!(check.entries[0].detail.as_deref(), Some("1 -> 2"));

    let outcome = sync_with_client(
        &mock,
        sync_options(&manifest, &roots, &cache_root, false, false),
    )
    .unwrap();
    assert_eq!(outcome.entries[0].action, SyncAction::Updated);
    assert_eq!(outcome.entries[0].version.as_deref(), Some("2"));

    let receipt: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(target_root.join("code-review/.agentstack-install.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["version"].as_str(), Some("2"));
}

#[test]
fn sync_keeps_pinned_skill_on_its_pin() {
    let scratch = unique_dir("pinned");
    let src_v1 = scratch.join("v1/code-review");
    let src_v2 = scratch.join("v2/code-review");
    make_skill(&src_v1, "code-review", "Use when reviewing code (v1)");
    make_skill(&src_v2, "code-review", "Use when reviewing code (v2)");

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &src_v1, "code-review");
    push_skill(&mock, &src_v2);
    mock.approve(&"acme/code-review".parse().unwrap(), "2")
        .unwrap();

    let target_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    let pinned = manifest(
        &scratch,
        "[[skills]]\nref = \"acme/code-review@1\"\ntarget = \"local\"\n",
    );
    let roots = vec![(InstallTarget::Local, target_root.clone())];

    let outcome = sync_with_client(
        &mock,
        sync_options(&pinned, &roots, &cache_root, false, false),
    )
    .unwrap();
    assert_eq!(outcome.entries[0].action, SyncAction::Installed);
    assert_eq!(outcome.entries[0].version.as_deref(), Some("1"));

    // The current approved version is 2, but the pinned entry stays on 1.
    let second = sync_with_client(
        &mock,
        sync_options(&pinned, &roots, &cache_root, false, false),
    )
    .unwrap();
    assert_eq!(second.entries[0].action, SyncAction::UpToDate);
    assert_eq!(second.entries[0].version.as_deref(), Some("1"));

    // Moving the pin in the manifest converges onto the new pin.
    let repinned = manifest(
        &scratch,
        "[[skills]]\nref = \"acme/code-review@2\"\ntarget = \"local\"\n",
    );
    let moved = sync_with_client(
        &mock,
        sync_options(&repinned, &roots, &cache_root, false, false),
    )
    .unwrap();
    assert_eq!(moved.entries[0].action, SyncAction::Updated);
    assert_eq!(moved.entries[0].version.as_deref(), Some("2"));
    assert_eq!(moved.entries[0].detail.as_deref(), Some("1 -> 2"));
}

#[test]
fn sync_prune_removes_undeclared_managed_installs_only() {
    let scratch = unique_dir("prune");
    let src_review = scratch.join("src/code-review");
    let src_extra = scratch.join("src/extra-skill");
    make_skill(&src_review, "code-review", "Use when reviewing code");
    make_skill(&src_extra, "extra-skill", "Use when extra");

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &src_review, "code-review");
    publish_approved_skill(&mock, &src_extra, "extra-skill");

    let target_root = scratch.join("target");
    let cache_root = scratch.join("cache");

    // A managed install the manifest does not declare.
    let extra_ref: SkillRef = "acme/extra-skill".parse().unwrap();
    install_remote_with_client(
        &mock,
        RemoteInstallOptions {
            skill_ref: &extra_ref,
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some(REGISTRY_URL),
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
            allow_yanked: false,
        },
    )
    .unwrap();

    // An unmanaged directory (no receipt) that prune must never touch.
    let unmanaged = target_root.join("notes");
    fs::create_dir_all(&unmanaged).unwrap();
    fs::write(unmanaged.join("README.md"), "hands off\n").unwrap();

    let manifest = manifest(
        &scratch,
        "[[skills]]\nref = \"acme/code-review\"\ntarget = \"local\"\n",
    );
    let roots = vec![(InstallTarget::Local, target_root.clone())];

    let check = sync_with_client(
        &mock,
        sync_options(&manifest, &roots, &cache_root, true, true),
    )
    .unwrap();
    let would_prune: Vec<_> = check
        .entries
        .iter()
        .filter(|entry| entry.action == SyncAction::WouldPrune)
        .collect();
    assert_eq!(would_prune.len(), 1, "{check:?}");
    assert_eq!(would_prune[0].entry_ref, "acme/extra-skill");
    assert!(
        target_root.join("extra-skill/SKILL.md").is_file(),
        "check must not remove anything"
    );

    let outcome = sync_with_client(
        &mock,
        sync_options(&manifest, &roots, &cache_root, false, true),
    )
    .unwrap();
    let pruned: Vec<_> = outcome
        .entries
        .iter()
        .filter(|entry| entry.action == SyncAction::Pruned)
        .collect();
    assert_eq!(pruned.len(), 1, "{outcome:?}");
    assert_eq!(pruned[0].entry_ref, "acme/extra-skill");
    assert!(!target_root.join("extra-skill").exists());
    assert!(
        unmanaged.join("README.md").is_file(),
        "prune must not touch unmanaged files"
    );
    assert!(target_root.join("code-review/SKILL.md").is_file());
}

#[test]
fn sync_prune_removes_undeclared_stack_and_its_owned_children() {
    let scratch = unique_dir("prune-stack");
    let src_review = scratch.join("src/code-review");
    let src_incident = scratch.join("src/incident-runbook");
    make_skill(&src_review, "code-review", "Use when reviewing code");
    make_skill(
        &src_incident,
        "incident-runbook",
        "Use when handling incidents",
    );

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &src_review, "code-review");
    publish_approved_skill(&mock, &src_incident, "incident-runbook");
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
    let roots = vec![(InstallTarget::Local, target_root.clone())];

    // Install the stack first, then sync a manifest that no longer declares it.
    let with_stack = manifest(
        &scratch,
        "[[stacks]]\nref = \"acme/engineering-default\"\ntarget = \"local\"\n\n\
         [[skills]]\nref = \"acme/code-review\"\ntarget = \"local\"\n",
    );
    sync_with_client(
        &mock,
        sync_options(&with_stack, &roots, &cache_root, false, false),
    )
    .unwrap();
    assert!(target_root.join("incident-runbook/SKILL.md").is_file());

    let without_stack = manifest(
        &scratch,
        "[[skills]]\nref = \"acme/code-review\"\ntarget = \"local\"\n",
    );
    let outcome = sync_with_client(
        &mock,
        sync_options(&without_stack, &roots, &cache_root, false, true),
    )
    .unwrap();
    let pruned: Vec<_> = outcome
        .entries
        .iter()
        .filter(|entry| entry.action == SyncAction::Pruned)
        .collect();
    assert_eq!(pruned.len(), 1, "{outcome:?}");
    assert_eq!(pruned[0].entry_ref, "acme/engineering-default");
    assert!(!target_root.join("incident-runbook").exists());
    assert!(
        !target_root
            .join(".agentstack-stacks/acme/engineering-default")
            .exists()
    );
    assert!(target_root.join("code-review/SKILL.md").is_file());
}

#[test]
fn sync_missing_manifest_fails_with_format_help() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    fs::create_dir_all(&cfg_dir).unwrap();
    let missing = tmp.path().join("agentstack.toml");

    let assert = assert_cmd::Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", &cfg_dir)
        .args(["sync", "--manifest", missing.to_str().unwrap()])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("not found"), "stderr: {stderr}");
    assert!(stderr.contains("[[stacks]]"), "stderr: {stderr}");
    assert!(stderr.contains("[[skills]]"), "stderr: {stderr}");
}

#[test]
fn sync_refuses_cross_registry_drift_restore() {
    let scratch = unique_dir("cross-registry");
    let src = scratch.join("src/code-review");
    make_skill(&src, "code-review", "Use when reviewing code");

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &src, "code-review");

    let target_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    let skill_ref: SkillRef = "acme/code-review".parse().unwrap();
    install_remote_with_client(
        &mock,
        RemoteInstallOptions {
            skill_ref: &skill_ref,
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some(REGISTRY_URL),
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
            allow_yanked: false,
        },
    )
    .unwrap();
    // Drift the install so sync's restore pass would run with force.
    let edited = target_root.join("code-review/SKILL.md");
    fs::write(&edited, "locally edited\n").unwrap();

    let manifest = manifest(
        &scratch,
        "[[skills]]\nref = \"acme/code-review\"\ntarget = \"local\"\n",
    );
    let roots = vec![(InstallTarget::Local, target_root.clone())];
    let mut opts = sync_options(&manifest, &roots, &cache_root, false, false);
    opts.registry_url = Some("mock://other-registry");

    let outcome = sync_with_client(&mock, opts).unwrap();
    assert_eq!(outcome.entries.len(), 1, "{outcome:?}");
    assert_eq!(outcome.entries[0].action, SyncAction::Failed);
    let detail = outcome.entries[0].detail.as_deref().unwrap_or_default();
    assert!(detail.contains("different registry"), "detail: {detail}");
    assert_eq!(
        fs::read_to_string(&edited).unwrap(),
        "locally edited\n",
        "a refused sync must not touch the install"
    );
}

#[test]
fn sync_skips_prune_when_an_entry_fails() {
    let scratch = unique_dir("prune-after-failure");
    let src_extra = scratch.join("src/extra-skill");
    make_skill(&src_extra, "extra-skill", "Use when extra");

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &src_extra, "extra-skill");

    let target_root = scratch.join("target");
    let cache_root = scratch.join("cache");
    let extra_ref: SkillRef = "acme/extra-skill".parse().unwrap();
    install_remote_with_client(
        &mock,
        RemoteInstallOptions {
            skill_ref: &extra_ref,
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some(REGISTRY_URL),
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
            allow_yanked: false,
        },
    )
    .unwrap();

    // The declared skill does not exist in the registry, so its entry fails;
    // the undeclared-but-managed extra-skill must then survive the prune.
    let manifest = manifest(
        &scratch,
        "[[skills]]\nref = \"acme/missing-skill\"\ntarget = \"local\"\n",
    );
    let roots = vec![(InstallTarget::Local, target_root.clone())];
    let outcome = sync_with_client(
        &mock,
        sync_options(&manifest, &roots, &cache_root, false, true),
    )
    .unwrap();

    assert_eq!(outcome.entries.len(), 1, "{outcome:?}");
    assert_eq!(outcome.entries[0].action, SyncAction::Failed);
    assert!(
        outcome.prune_skipped.is_some(),
        "prune must be skipped after a failed entry: {outcome:?}"
    );
    assert!(
        !outcome
            .entries
            .iter()
            .any(|entry| entry.action == SyncAction::Pruned),
        "{outcome:?}"
    );
    assert!(
        target_root.join("extra-skill/SKILL.md").is_file(),
        "undeclared install must survive a prune pass that follows a failure"
    );
}

#[cfg(unix)]
#[test]
fn sync_prune_ignores_symlinked_dirs() {
    let scratch = unique_dir("prune-symlink");
    let src_review = scratch.join("src/code-review");
    let src_extra = scratch.join("src/extra-skill");
    make_skill(&src_review, "code-review", "Use when reviewing code");
    make_skill(&src_extra, "extra-skill", "Use when extra");

    let mock = MockRegistryClient::with_user("octocat");
    publish_approved_skill(&mock, &src_review, "code-review");
    publish_approved_skill(&mock, &src_extra, "extra-skill");

    let target_root = scratch.join("target");
    let outside_root = scratch.join("outside");
    let cache_root = scratch.join("cache");

    // Manifest-declared install so the target root exists and is managed.
    let review_ref: SkillRef = "acme/code-review".parse().unwrap();
    install_remote_with_client(
        &mock,
        RemoteInstallOptions {
            skill_ref: &review_ref,
            dest_root: &target_root,
            target: "local",
            force: false,
            registry_url: Some(REGISTRY_URL),
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
            allow_yanked: false,
        },
    )
    .unwrap();
    // A managed install outside the target root, reachable only through a
    // symlink inside it. Prune must treat the symlink as unmanaged.
    let extra_ref: SkillRef = "acme/extra-skill".parse().unwrap();
    install_remote_with_client(
        &mock,
        RemoteInstallOptions {
            skill_ref: &extra_ref,
            dest_root: &outside_root,
            target: "local",
            force: false,
            registry_url: Some(REGISTRY_URL),
            installed_by: Some("octocat".to_string()),
            cache_root: Some(&cache_root),
            allow_yanked: false,
        },
    )
    .unwrap();
    std::os::unix::fs::symlink(
        outside_root.join("extra-skill"),
        target_root.join("extra-skill"),
    )
    .unwrap();

    let manifest = manifest(
        &scratch,
        "[[skills]]\nref = \"acme/code-review\"\ntarget = \"local\"\n",
    );
    let roots = vec![(InstallTarget::Local, target_root.clone())];
    let outcome = sync_with_client(
        &mock,
        sync_options(&manifest, &roots, &cache_root, false, true),
    )
    .unwrap();

    assert!(
        !outcome
            .entries
            .iter()
            .any(|entry| entry.action == SyncAction::Pruned),
        "symlinked dirs must never be pruned: {outcome:?}"
    );
    assert!(
        outside_root.join("extra-skill/SKILL.md").is_file(),
        "prune must not delete through a symlink"
    );
    assert!(target_root.join("extra-skill").exists());
}
