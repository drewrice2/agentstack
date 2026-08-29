use std::fs;
use std::io::Write;

use agentstack::package::{MAX_ARCHIVE_ENTRIES, MAX_EXTRACTED_FILE_BYTES};
use assert_cmd::Command;
use assert_fs::TempDir;
use assert_fs::fixture::ChildPath;
use assert_fs::prelude::*;
use predicates::prelude::*;

const SKILL_MD: &str = "\
---
name: round-trip
description: Use when round-tripping
---

# Purpose

Body.

# When to Use

# Instructions

# Output

# Boundaries
";

fn make_skill(parent: &TempDir) -> ChildPath {
    let target = parent.child("round-trip");
    target.create_dir_all().unwrap();
    target.child("SKILL.md").write_str(SKILL_MD).unwrap();
    target
        .child("references/notes.md")
        .write_str("notes body")
        .unwrap();
    target
}

fn pack(skill: &ChildPath, archive: &ChildPath, cache: &ChildPath) {
    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args([
            "skill",
            "pack",
            skill.path().to_str().unwrap(),
            "--out",
            archive.path().to_str().unwrap(),
        ])
        .assert()
        .success();
}

fn validate(path: &ChildPath) {
    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "validate", path.path().to_str().unwrap()])
        .assert()
        .success();
}

fn lint(path: &ChildPath) {
    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "lint", path.path().to_str().unwrap()])
        .assert()
        .success();
}

fn write_targz(entries: &[ArchiveEntry<'_>], archive: &ChildPath) {
    let file = fs::File::create(archive.path()).unwrap();
    let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);

    for entry in entries {
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_entry_type(entry.entry_type);
        header.set_size(entry.contents.len() as u64);
        header.set_path(entry.path).unwrap();
        if let Some(link_name) = entry.link_name {
            header.set_link_name(link_name).unwrap();
        }
        header.set_cksum();
        tar.append(&header, entry.contents).unwrap();
    }

    let gz = tar.into_inner().unwrap();
    gz.finish().unwrap();
}

fn write_many_entry_targz(count: usize, archive: &ChildPath) {
    let file = fs::File::create(archive.path()).unwrap();
    let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);

    for index in 0..count {
        let path = if index == 0 {
            "round-trip/SKILL.md".to_string()
        } else {
            format!("round-trip/references/{index}.txt")
        };
        let contents = if index == 0 {
            SKILL_MD.as_bytes()
        } else {
            b"x".as_slice()
        };
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(contents.len() as u64);
        header.set_path(path).unwrap();
        header.set_cksum();
        tar.append(&header, contents).unwrap();
    }

    let gz = tar.into_inner().unwrap();
    gz.finish().unwrap();
}

fn write_raw_targz(entries: &[(&str, &[u8])], archive: &ChildPath) {
    let file = fs::File::create(archive.path()).unwrap();
    let mut gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());

    for (path, contents) in entries {
        let mut header = [0u8; 512];
        let path_bytes = path.as_bytes();
        assert!(path_bytes.len() <= 100);
        header[..path_bytes.len()].copy_from_slice(path_bytes);
        write_octal(&mut header[100..108], 0o644);
        write_octal(&mut header[108..116], 0);
        write_octal(&mut header[116..124], 0);
        write_octal(&mut header[124..136], contents.len() as u64);
        write_octal(&mut header[136..148], 0);
        header[148..156].fill(b' ');
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum: u32 = header.iter().map(|b| *b as u32).sum();
        let checksum_text = format!("{checksum:06o}\0 ");
        header[148..156].copy_from_slice(checksum_text.as_bytes());

        gz.write_all(&header).unwrap();
        gz.write_all(contents).unwrap();
        let padding = (512 - (contents.len() % 512)) % 512;
        if padding > 0 {
            gz.write_all(&vec![0u8; padding]).unwrap();
        }
    }

    gz.write_all(&[0u8; 1024]).unwrap();
    gz.finish().unwrap();
}

fn write_octal(slot: &mut [u8], value: u64) {
    let text = format!("{value:0width$o}\0", width = slot.len() - 1);
    slot.copy_from_slice(text.as_bytes());
}

struct ArchiveEntry<'a> {
    path: &'a str,
    contents: &'a [u8],
    entry_type: tar::EntryType,
    link_name: Option<&'a str>,
}

#[test]
fn unpack_round_trips_through_pack() {
    let tmp = TempDir::new().unwrap();
    let skill = make_skill(&tmp);
    let cache = tmp.child("cache");
    let archive = tmp.child("round-trip.tar.gz");
    pack(&skill, &archive, &cache);

    let out_parent = tmp.child("extracted");
    let out = out_parent.child("round-trip");
    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args([
            "skill",
            "unpack",
            archive.path().to_str().unwrap(),
            "--out",
            out_parent.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("unpacked "))
        .stdout(predicate::str::contains("sha256:"))
        .stdout(predicate::str::contains("next:"));

    out.child("SKILL.md").assert(predicate::path::is_file());
    out_parent
        .child("SKILL.md")
        .assert(predicate::path::missing());
    out.child("references/notes.md")
        .assert(predicate::path::is_file());

    // The unpacked directory must validate.
    validate(&out);
}

#[test]
fn init_pack_unpack_validate_lint_round_trip() {
    let tmp = TempDir::new().unwrap();
    let skill = tmp.child("ad-test");
    let cache = tmp.child("cache");
    let archive = tmp.child("ad-test.agentstack.tar.gz");
    let unpack_parent = tmp.child("unpacked-parent");
    let unpacked = unpack_parent.child("ad-test");

    Command::cargo_bin("agentstack")
        .unwrap()
        .args([
            "skill",
            "init",
            skill.path().to_str().unwrap(),
            "--name",
            "ad-test",
            "--description",
            "Use when testing AgentStack CLI behavior.",
        ])
        .assert()
        .success();

    skill
        .child("SKILL.md")
        .write_str(
            "---\nname: ad-test\ndescription: Use when testing AgentStack CLI behavior.\n---\n\n# Purpose\n\nExercise pack and unpack round trips.\n\n# When to Use\n\nUse when validating archive behavior.\n\n# Instructions\n\nPack and unpack the skill.\n\n# Output\n\nA validated unpacked skill.\n\n# Boundaries\n\nDo not contact a registry.\n",
        )
        .unwrap();

    validate(&skill);
    lint(&skill);
    pack(&skill, &archive, &cache);

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args([
            "skill",
            "unpack",
            archive.path().to_str().unwrap(),
            "--out",
            unpack_parent.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    validate(&unpacked);
    lint(&unpacked);
}

#[test]
fn pack_unpack_preserves_empty_standard_directories() {
    let tmp = TempDir::new().unwrap();
    let skill = tmp.child("empty-dirs");
    skill.create_dir_all().unwrap();
    skill
        .child("SKILL.md")
        .write_str(
            "---\nname: empty-dirs\ndescription: Use when testing empty directory preservation.\n---\n\n# Purpose\n\n# When to Use\n\n# Instructions\n\n# Output\n\n# Boundaries\n",
        )
        .unwrap();
    for sub in ["references", "examples", "assets", "scripts", "platform"] {
        skill.child(sub).create_dir_all().unwrap();
    }

    let cache = tmp.child("cache");
    let archive = tmp.child("empty-dirs.tar.gz");
    let unpack_parent = tmp.child("unpacked");
    let unpacked = unpack_parent.child("empty-dirs");
    pack(&skill, &archive, &cache);

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args([
            "skill",
            "unpack",
            archive.path().to_str().unwrap(),
            "--out",
            unpack_parent.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    for sub in ["references", "examples", "assets", "scripts", "platform"] {
        unpacked.child(sub).assert(predicate::path::is_dir());
    }
    validate(&unpacked);
    lint(&unpacked);
}

#[test]
fn unpack_refuses_non_empty_destination_without_force() {
    let tmp = TempDir::new().unwrap();
    let skill = make_skill(&tmp);
    let cache = tmp.child("cache");
    let archive = tmp.child("pkg.tar.gz");
    pack(&skill, &archive, &cache);

    let out_parent = tmp.child("dest");
    let out = out_parent.child("round-trip");
    out.create_dir_all().unwrap();
    out.child("placeholder.txt").write_str("hands off").unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args([
            "skill",
            "unpack",
            archive.path().to_str().unwrap(),
            "--out",
            out_parent.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--force"));

    out.child("placeholder.txt")
        .assert(predicate::path::is_file());

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args([
            "skill",
            "unpack",
            archive.path().to_str().unwrap(),
            "--out",
            out_parent.path().to_str().unwrap(),
            "--force",
        ])
        .assert()
        .success();
    out.child("SKILL.md").assert(predicate::path::is_file());
    out.child("placeholder.txt")
        .assert(predicate::path::missing());
}

#[test]
fn unpack_force_replaces_only_final_skill_directory() {
    let tmp = TempDir::new().unwrap();
    let skill = make_skill(&tmp);
    let cache = tmp.child("cache");
    let archive = tmp.child("pkg.tar.gz");
    pack(&skill, &archive, &cache);

    let out_parent = tmp.child("parent");
    let out = out_parent.child("round-trip");
    out.create_dir_all().unwrap();
    out.child("stale.txt").write_str("old").unwrap();
    out_parent.child("keep.txt").write_str("keep").unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args([
            "skill",
            "unpack",
            archive.path().to_str().unwrap(),
            "--out",
            out_parent.path().to_str().unwrap(),
            "--force",
        ])
        .assert()
        .success();

    out.child("SKILL.md").assert(predicate::path::is_file());
    out.child("stale.txt").assert(predicate::path::missing());
    out_parent
        .child("keep.txt")
        .assert(predicate::path::is_file());
    let backup_count = fs::read_dir(out_parent.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".agentstack-unpack-backup-")
        })
        .count();
    assert_eq!(backup_count, 0);
}

#[test]
fn unpack_force_rejects_ancestor_destination_without_deleting_it() {
    let tmp = TempDir::new().unwrap();
    let skill = make_skill(&tmp);
    let cache = tmp.child("cache");
    let archive = tmp.child("pkg.tar.gz");
    pack(&skill, &archive, &cache);

    let parent = tmp.child("parent");
    let final_dir = parent.child("round-trip");
    let work = final_dir.child("work");
    work.create_dir_all().unwrap();
    let keep = final_dir.child("keep.txt");
    keep.write_str("keep").unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(work.path())
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args([
            "skill",
            "unpack",
            archive.path().to_str().unwrap(),
            "--out",
            "../..",
            "--force",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("ancestor of current directory"));

    keep.assert(predicate::path::is_file());
    work.assert(predicate::path::is_dir());
}

#[test]
fn unpack_rejects_corrupt_archive() {
    let tmp = TempDir::new().unwrap();
    let bogus = tmp.child("bogus.tar.gz");
    bogus.write_str("not a real archive").unwrap();
    let out = tmp.child("dest");

    Command::cargo_bin("agentstack")
        .unwrap()
        .args([
            "skill",
            "unpack",
            bogus.path().to_str().unwrap(),
            "--out",
            out.path().to_str().unwrap(),
        ])
        .assert()
        .failure();
}

#[test]
fn unpack_rejects_path_traversal_archive() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.child("traversal.tar.gz");
    write_raw_targz(
        &[
            ("round-trip/SKILL.md", SKILL_MD.as_bytes()),
            ("round-trip/../evil.txt", b"bad"),
        ],
        &archive,
    );
    let out = tmp.child("dest");

    Command::cargo_bin("agentstack")
        .unwrap()
        .args([
            "skill",
            "unpack",
            archive.path().to_str().unwrap(),
            "--out",
            out.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unsafe path"));

    tmp.child("evil.txt").assert(predicate::path::missing());
    out.assert(predicate::path::missing());
}

#[test]
fn unpack_accepts_arbitrary_support_files_and_directories() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.child("support-entry.tar.gz");
    write_targz(
        &[
            ArchiveEntry {
                path: "round-trip/SKILL.md",
                contents: SKILL_MD.as_bytes(),
                entry_type: tar::EntryType::Regular,
                link_name: None,
            },
            ArchiveEntry {
                path: "round-trip/reference.md",
                contents: b"# reference",
                entry_type: tar::EntryType::Regular,
                link_name: None,
            },
            ArchiveEntry {
                path: "round-trip/LICENSE.txt",
                contents: b"Copyright example",
                entry_type: tar::EntryType::Regular,
                link_name: None,
            },
            ArchiveEntry {
                path: "round-trip/templates/template.md",
                contents: b"# template",
                entry_type: tar::EntryType::Regular,
                link_name: None,
            },
            ArchiveEntry {
                path: "round-trip/agents/openai.yaml",
                contents: b"name: round-trip",
                entry_type: tar::EntryType::Regular,
                link_name: None,
            },
        ],
        &archive,
    );
    let out = tmp.child("dest");

    Command::cargo_bin("agentstack")
        .unwrap()
        .args([
            "skill",
            "unpack",
            archive.path().to_str().unwrap(),
            "--out",
            out.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let unpacked = out.child("round-trip");
    unpacked
        .child("reference.md")
        .assert(predicate::path::is_file());
    unpacked
        .child("LICENSE.txt")
        .assert(predicate::path::is_file());
    unpacked
        .child("templates/template.md")
        .assert(predicate::path::is_file());
    unpacked
        .child("agents/openai.yaml")
        .assert(predicate::path::is_file());
}

#[test]
fn unpack_rejects_excluded_package_file() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.child("excluded-file.tar.gz");
    write_targz(
        &[
            ArchiveEntry {
                path: "round-trip/SKILL.md",
                contents: SKILL_MD.as_bytes(),
                entry_type: tar::EntryType::Regular,
                link_name: None,
            },
            ArchiveEntry {
                path: "round-trip/references/.env",
                contents: b"TOKEN=secret",
                entry_type: tar::EntryType::Regular,
                link_name: None,
            },
        ],
        &archive,
    );
    let out = tmp.child("dest");

    Command::cargo_bin("agentstack")
        .unwrap()
        .args([
            "skill",
            "unpack",
            archive.path().to_str().unwrap(),
            "--out",
            out.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("excluded package file"));

    out.assert(predicate::path::missing());
}

#[test]
fn unpack_rejects_archive_symlink_entries() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.child("symlink-entry.tar.gz");
    write_targz(
        &[
            ArchiveEntry {
                path: "round-trip/SKILL.md",
                contents: SKILL_MD.as_bytes(),
                entry_type: tar::EntryType::Regular,
                link_name: None,
            },
            ArchiveEntry {
                path: "round-trip/link",
                contents: b"",
                entry_type: tar::EntryType::Symlink,
                link_name: Some("../outside"),
            },
        ],
        &archive,
    );
    let out = tmp.child("dest");

    Command::cargo_bin("agentstack")
        .unwrap()
        .args([
            "skill",
            "unpack",
            archive.path().to_str().unwrap(),
            "--out",
            out.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unsupported entry type"));

    out.assert(predicate::path::missing());
}

#[test]
fn unpack_rejects_archive_with_too_many_entries() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.child("too-many.tar.gz");
    write_many_entry_targz(MAX_ARCHIVE_ENTRIES + 1, &archive);
    let out = tmp.child("dest");

    Command::cargo_bin("agentstack")
        .unwrap()
        .args([
            "skill",
            "unpack",
            archive.path().to_str().unwrap(),
            "--out",
            out.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("entries"));

    out.assert(predicate::path::missing());
}

#[test]
fn unpack_rejects_oversized_file_entry() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.child("oversized-file.tar.gz");
    let large = vec![b'x'; MAX_EXTRACTED_FILE_BYTES as usize + 1];
    write_targz(
        &[
            ArchiveEntry {
                path: "round-trip/SKILL.md",
                contents: SKILL_MD.as_bytes(),
                entry_type: tar::EntryType::Regular,
                link_name: None,
            },
            ArchiveEntry {
                path: "round-trip/references/large.bin",
                contents: &large,
                entry_type: tar::EntryType::Regular,
                link_name: None,
            },
        ],
        &archive,
    );
    let out = tmp.child("dest");

    Command::cargo_bin("agentstack")
        .unwrap()
        .args([
            "skill",
            "unpack",
            archive.path().to_str().unwrap(),
            "--out",
            out.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("per-file limit"));

    out.assert(predicate::path::missing());
}

#[cfg(unix)]
#[test]
fn unpack_rejects_symlink_destination() {
    let tmp = TempDir::new().unwrap();
    let skill = make_skill(&tmp);
    let cache = tmp.child("cache");
    let archive = tmp.child("pkg.tar.gz");
    pack(&skill, &archive, &cache);

    let real = tmp.child("real-dest");
    real.create_dir_all().unwrap();
    let link = tmp.child("dest-link");
    std::os::unix::fs::symlink(real.path(), link.path()).unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args([
            "skill",
            "unpack",
            archive.path().to_str().unwrap(),
            "--out",
            link.path().to_str().unwrap(),
            "--force",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("symlink"));

    real.child("SKILL.md").assert(predicate::path::missing());
}
