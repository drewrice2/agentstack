# `agentstack` command reference

Canonical CLI contract. Command help owns examples. This file owns names,
semantics, JSON, environment, and exits.

Agents: prefer `--json` and `--no-input`. Never pass a token as a CLI
argument. Load `examples/skills/agentstack` for the safe operating path.

## Global Options

```text
agentstack [GLOBAL OPTIONS] <NAMESPACE> <COMMAND> [COMMAND OPTIONS]
```

| Flag | Contract |
| --- | --- |
| `--json` | Emit stable machine-readable JSON where supported. Canonical action, list, status, and mutation commands support it; `completion` and path-only printers such as `config path` and `cache path` remain plain stdout. JSON mode never prompts. |
| `--no-input` | Do not prompt; fail with a concrete next command instead. |
| `--verbose`, `-v` | Diagnostic detail on stderr. |
| `--quiet`, `-q` | Suppress non-essential human output. |
| `--help`, `-h` | Show help. |
| `--version`, `-V` | Print version. |

## Canonical Namespaces

Canonical resource namespaces are singular:

`skill`, `stack`, `team`, `install`, `auth`, `registry`, `target`, `audit`,
`config`, `cache`, `sync`, `doctor`, and `completion`.

Do not add shorthand command aliases such as `ls` or `promote`.

## Command Classes

| Class | Canonical commands |
| --- | --- |
| Local authoring/filesystem | `skill init`, `skill validate`, `skill lint`, `skill inspect`, `skill security-scan`, `skill scan`, `skill pack`, `skill unpack`, `skill diff`, `cache path`, `cache list`, `cache remove`, `config show`, `target list`, `target detect`, `target path` |
| Remote registry inspection | `skill list`, `skill search`, `skill candidates`, `skill show`, `skill status`, `skill impact`, `skill diff`, `skill export`, `skill audit`, `skill visibility show`, `skill version list`, `skill version show`, `stack list`, `stack show`, `stack status`, `stack resolve`, `stack export`, `stack audit`, `team list`, `team inspect`, `registry show`, `registry ping`, `auth whoami`, `audit list`, `audit show` |
| Local auth inspection | `auth status` |
| Remote registry mutation | `skill push`, `skill adopt`, `skill visibility set`, `skill version approve`, `skill version yank`, `skill version deprecate`, `stack create`, `stack add`, `stack remove`, `stack visibility set`, `team create`, `team add-member`, `team remove-member`, `team set-role` |
| Installed runtime state | `skill install`, `skill show --target`, `skill update`, `skill uninstall`, `stack install`, `stack show --target`, `stack update`, `stack uninstall`, `sync` |
| Receipt administration | `install list`, `install why`, `install update --all`, `install doctor`, `install unlock` |
| Configuration/context | `registry show`, `registry use`, `config show`, `target list`, `target detect`, `target path`, `target setup`, `target set`, `target unset` |
| Diagnostics | `doctor`, `registry ping`, `install doctor`, `cache list`, `completion` |

## Role Matrix

Hosted registry tokens have fixed org roles. Choose the path that matches the
token instead of assuming one token can do everything.

| Role | Can do | Should not do |
| --- | --- | --- |
| `reader` | `registry ping --auth`, `auth whoami`, `skill list/search/show/status/export/install/show --target/update --check`, `stack list/show/status/resolve/export/install/show --target/update --check` for visible approved resources | `skill push`, approval, visibility changes, stack mutation, yanks/deprecations |
| team member | Reader abilities for team-visible skills and stacks in that team | Membership changes, approval, visibility changes, org-wide admin work |
| team admin | Team member abilities plus inspection of that team and, for team-scoped resources: `skill version approve`, yanks/deprecations, visibility changes, and stack create/add/remove | Approval, lifecycle, or visibility changes for org- or private-scoped skills; org-wide admin work; configurable policy changes |
| `publisher` | Everything a reader can do, plus `skill push` and `skill adopt` candidate uploads for the org; publisher team members can push team-scoped candidates with `--scope team --team <TEAM>` | `skill version approve`, yanks/deprecations, stack create/add/remove, org-wide admin work unless also team admin or org admin |
| `org_admin` | Publisher commands plus approval/current promotion, yanks/deprecations, visibility changes, stack create/add/remove, team membership management, and org audit review | Server infrastructure or cross-org work unless separately granted |
| machine token | Usually headless reader/export/install paths using `AGENTSTACK_REGISTRY_URL` with `AGENTSTACK_TOKEN_PATH` or `AGENTSTACK_TOKEN`; may be publisher-scoped only when explicitly issued that way | Interactive `auth login`, local credential-store setup, broad mutation unless the token was issued for that |

Use env-only auth for CI and headless agents:

```bash
AGENTSTACK_REGISTRY_URL=https://registry.agentstack.gg \
AGENTSTACK_TOKEN="$CI_AGENTSTACK_TOKEN" \
AGENTSTACK_NONINTERACTIVE=1 \
agentstack stack export acme/engineering-default --out ./skills --dry-run --json
```

## Local vs Remote Semantics

`skill list` means registry list. It lists skills visible to the active token,
optionally filtered by org, team, platform, scope, owner, and limit. `skill
search` adds a query on top of the same catalog filters. Skill catalog rows can
be sorted by `name`, `updated`, `owner`, or `installs`.

`skill scan` is local discovery. It scans the current directory, or a supplied
path, for skill directories. There is no canonical `--local` inversion.

`stack list` means registry list. Stacks are registry resources.

## Canonical Command Map

### Skill

| Command | Purpose |
| --- | --- |
| `agentstack skill init [PATH] --name <NAME> --description <DESC>` | Scaffold a skill directory. |
| `agentstack skill validate [PATH]` | Enforce hard skill-format rules. |
| `agentstack skill lint [PATH] [--max-chars N]` | Run advisory quality checks. |
| `agentstack skill inspect [PATH] [--max-chars N]` | Summarize metadata, structure, errors, warnings, and package hash. |
| `agentstack skill security-scan [PATH]` | Run narrow local static checks for risky prompt text. |
| `agentstack skill scan [PATH]` | Discover local skill directories. |
| `agentstack skill pack [PATH] [--out FILE] [--force] [--no-cache]` | Create a deterministic archive. Packing still succeeds with lint warnings, but human output warns and points to `agentstack skill lint <path>`. |
| `agentstack skill unpack <ARCHIVE> --out <PARENT_DIR> [--force]` | Extract an archive into `<parent>/<skill-name>`. |
| `agentstack skill push [PATH] [--org ORG] [--scope private\|org\|team] [--team TEAM] [--platform TAG]... [--dry-run] [--yes]` | Upload a candidate skill version. Real uploads require confirmation; use `--yes` for non-interactive callers. Dry-run validates and packages locally but does not check registry authorization. Omit `--org` when the active token has one org. |
| `agentstack skill push --all [PATH] [--org ORG] [--include GLOB]... [--exclude GLOB]... [--dry-run] [--yes]` | Batch-push direct child skill directories. Omit `--org` when the active token has one org. |
| `agentstack skill adopt [PATH] [--org ORG] [--scope private\|org\|team] [--team TEAM] [--platform TAG]... [--dry-run] [--yes]` | Scan a directory for skills, confirm a plan, and push the valid ones as candidate versions. Invalid skills are skipped with reasons; per-skill push failures do not abort the batch. Exits nonzero when at least one push was attempted and none succeeded, or on a preflight failure (bad arguments, unreadable path, auth/registry errors). |
| `agentstack skill list [--org ORG] [--platform TAG]... [--scope private\|org\|team] [--team TEAM] [--owner EMAIL] [--sort name\|updated\|owner\|installs] [--limit N]` | List visible registry skills. |
| `agentstack skill search <QUERY> [--org ORG] [--platform TAG]... [--scope private\|org\|team] [--team TEAM] [--owner EMAIL] [--sort name\|updated\|owner\|installs] [--limit N]` | Search visible registry skills. |
| `agentstack skill candidates [--org ORG] [--limit N]` | List non-yanked candidate versions awaiting approval across visible skills, newest first, with copy-ready approve commands. Aggregated client-side; `--limit` caps skills scanned (default 100) with an explicit truncation note. |
| `agentstack skill show <SKILL[@VERSION]\|ORG/SKILL[@VERSION]> [--team TEAM]` | Show registry metadata for a skill or pinned version. |
| `agentstack skill status <SKILL\|ORG/SKILL> [--team TEAM]` | Show lifecycle and current-version status. |
| `agentstack skill impact <SKILL\|ORG/SKILL> [--team TEAM]` | Show visible stacks that currently or explicitly reference a skill. |
| `agentstack skill diff <LEFT> [RIGHT] [--target TARGET] [--allow-yanked]` | Compare packaged file contents for local skill paths, registry refs, or an installed copy. Pass two refs, or `--target` to compare an installed skill against the registry. `--allow-yanked` is server-admin recovery only. |
| `agentstack skill visibility show <SKILL\|ORG/SKILL> [--team TEAM]` | Show current visibility scope. |
| `agentstack skill visibility set <SKILL\|ORG/SKILL> --scope private\|org\|team [--team TEAM]` | Change visibility without approving or moving versions. |
| `agentstack skill audit <SKILL\|ORG/SKILL> [--team TEAM]` | List audit events for one skill. |
| `agentstack skill version list <SKILL\|ORG/SKILL> [--team TEAM]` | List uploaded versions visible to the caller. |
| `agentstack skill version show <SKILL@VERSION\|ORG/SKILL@VERSION> [--team TEAM]` | Show one uploaded version. |
| `agentstack skill version approve <SKILL@VERSION\|ORG/SKILL@VERSION> [--team TEAM]` | Mark one uploaded version approved and current. |
| `agentstack skill version yank <SKILL@VERSION\|ORG/SKILL@VERSION> [--team TEAM] --reason <REASON>` | Mark a version unsafe for default install/export. |
| `agentstack skill version deprecate <SKILL@VERSION\|ORG/SKILL@VERSION> [--team TEAM] --reason <REASON>` | Mark a version superseded but still available. |
| `agentstack skill install <SKILL[@VERSION]\|ORG/SKILL[@VERSION]> [--team TEAM] --target <TARGET> [--force] [--allow-yanked]` | Install a registry skill and write a receipt. A pinned ref installs that version. `--allow-yanked` is server-admin recovery only and requires a pinned ref. |
| `agentstack skill install <PATH> --target <TARGET> [--force]` | Install a local skill and write a receipt. |
| `agentstack skill show <SKILL> --target <TARGET>` | Inspect the installed copy of a skill in one target. |
| `agentstack skill update <SKILL> --target <TARGET> [--check] [--force]` | Update an installed registry skill in one target. |
| `agentstack skill uninstall <SKILL> --target <TARGET> [--force] [--yes] [--dry-run]` | Uninstall a skill from one target. |
| `agentstack skill export <SKILL[@VERSION]\|ORG/SKILL[@VERSION]> [--team TEAM] --out <DIR> [--force] [--dry-run] [--allow-yanked]` | Write unmanaged skill files without receipts. `--allow-yanked` is server-admin recovery only and requires a pinned ref. |

`<TARGET>` is one of the built-in target names: `claude-code`, `codex`,
`repo-claude-code`, `repo-codex`, or `local`. The CLI also accepts the
phrase-order aliases `claude-code-repo` and `codex-repo`; receipts and config
continue to store the stable IDs `repo-claude-code` and `repo-codex`.

Use `codex` or `claude-code` for user-level runtime installs. Use
`codex-repo` or `claude-code-repo` for installs into the current repository.

User-level targets `claude-code` and `codex` must be configured before first
install with `agentstack target setup <TARGET> --yes` or
`agentstack target set <TARGET> --path <ABSOLUTE_PATH>`. Repo-scoped targets
and `local` can auto-register their default path on first use when no override
exists. A single `note: registered target ... -> ...` line is printed to
stderr for those auto-registrations; suppressed under `--json` and `--quiet`.
Subsequent installs reuse the recorded override.

Bare registry refs such as `code-review` and `code-review@2` are accepted when
the active token resolves to exactly one org. If the token can access multiple
orgs, interactive commands ask which org to use; non-interactive commands fail
with guidance to use `org/name`. Use `--team TEAM` with a bare ref to select a
team-visible resource. Slash-qualified refs always mean `org/name`, not
`team/name`; receipts and JSON output remain org-qualified.

`skill push` defaults to `--scope private`. `--scope team` requires
`--team <TEAM>` and limits approved reads to that team plus admins and the
owner.

For 100+ skill catalogs, start with a scoped list instead of a full catalog
dump:

```bash
agentstack skill list --org acme --platform codex --scope org --limit 50
agentstack skill list --org acme --owner owner@example.com --sort updated --limit 50
agentstack skill search review --org acme --platform codex --limit 25
agentstack skill status acme/code-review
agentstack skill impact acme/code-review
```

Human list/search output includes the V0 `owner` contact so a reader can find
the accountable publisher without opening audit history. JSON exposes the same
value as `owner_email`. JSON list/search rows also expose `updated_at`, a
registry-set UTC RFC3339 timestamp in `YYYY-MM-DDTHH:MM:SSZ` form. `--sort
updated` sorts newest-first by the same `updated_at` value returned in each
row, `--sort owner` groups accountability, and `--sort name` is the default
stable catalog order. Use `--owner` to narrow accountability review. Full
pagination, total counts, status filters, ownership transfer, and
multi-maintainer workflows are deferred.

Skill metadata may carry optional install metrics (`install_count`,
`last_installed_at`). When any row has them, human list/search output adds an
INSTALLS column and `skill show` prints an `installs:` line. `--sort installs`
sorts by `install_count` descending with metric-less rows last; servers
predating install metrics reject `sort=installs`.

`skill diff` accepts any mix of local skill paths and registry refs:

```bash
agentstack skill diff ./my-skill ./my-skill-v2
agentstack skill diff ./my-skill acme/code-review@2 --json
agentstack skill diff acme/code-review@1 acme/code-review@2
agentstack skill diff code-review --target codex-repo
agentstack skill diff acme/code-review@3 --target claude-code-repo
```

With `--target`, the left ref names an installed skill in that target (located
via its receipt) and the right side is the skill's current approved registry
version, or the pinned `@VERSION` when the ref carries one. `RIGHT` must be
omitted when `--target` is set.

The command compares the files that would be included in an AgentStack package.
Human output reports added, removed, changed, and unchanged file counts plus
changed file hashes. JSON exposes `left`, `right`, `added`, `removed`,
`changed`, `unchanged_count`, `changed_count`, and `is_empty`. Registry refs are
downloaded through the normal authenticated registry path, hash-checked against
metadata, unpacked into a temporary directory using the hardened archive
extractor, and removed after comparison. `--allow-yanked` is a server-admin
recovery path only and is valid only with explicit pinned registry refs such as
`acme/code-review@2`; ordinary readers should ask an admin to approve a
replacement instead of using yanked archives.

`skill security-scan` is a narrow local static scan of readable UTF-8 files in
a skill-like directory. It is best-effort: validation errors are reported and
still fail the command, but they do not prevent the scanner from inspecting
readable text files. Hidden files, secret-looking files, common dependency/build
directories, symlinks, binary files, and oversized files are skipped. The scan
flags prompt-injection phrases, exfiltration instructions, hidden-instruction
language, remote-download-to-shell patterns, suspicious shortened or paste/tunnel
links, and common secret-path references. High-severity findings fail the scan
(non-zero exit) so CI can gate on them; medium and low findings are reported as
advisories and do not fail the scan. In JSON, `ok` is true iff there are no
validation errors and no high-severity findings. It is not a comprehensive
security review, model behavior guarantee, malware scanner, or hosted registry
policy engine.

### Stack

| Command | Purpose |
| --- | --- |
| `agentstack stack create <STACK\|ORG/STACK> [--scope private\|org\|team] [--team TEAM] [--name NAME] [--description TEXT]` | Create a registry stack. |
| `agentstack stack list [--org ORG] [--team TEAM] [--owner EMAIL] [--limit N]` | List visible stacks in an org. Omit `--org` when the active token has one org. |
| `agentstack stack show <STACK\|ORG/STACK> [--team TEAM]` | Inspect a stack definition. |
| `agentstack stack status <STACK\|ORG/STACK> [--team TEAM]` | Show stack lifecycle status. |
| `agentstack stack add <STACK\|ORG/STACK> [--team TEAM] <SKILL[@VERSION]\|ORG/SKILL[@VERSION]> [--version-policy current\|pinned] [--pin-version VERSION]` | Add or update a stack item. |
| `agentstack stack remove <STACK\|ORG/STACK> [--team TEAM] <SKILL\|ORG/SKILL> [--yes] [--dry-run]` | Remove a skill from a registry stack definition. Real removals require confirmation; use `--yes` for non-interactive callers. `--dry-run` resolves the stack and reports what would be removed without changing it. |
| `agentstack stack resolve <STACK\|ORG/STACK> [--team TEAM]` | Resolve a stack to concrete skill versions and hashes. |
| `agentstack stack export <STACK\|ORG/STACK> [--team TEAM] --out <DIR> [--force] [--dry-run]` | Write unmanaged child skill files without receipts. |
| `agentstack stack install <STACK\|ORG/STACK> [--team TEAM] --target <TARGET> [--force]` | Install resolved child skills and write stack receipts. |
| `agentstack stack show <STACK\|ORG/STACK> --target <TARGET>` | Inspect the installed copy of a stack in one target. |
| `agentstack stack update <STACK\|ORG/STACK> --target <TARGET> [--check] [--force] [--prune]` | Update an installed registry stack in one target. |
| `agentstack stack uninstall <STACK\|ORG/STACK> --target <TARGET> [--force] [--yes] [--dry-run]` | Uninstall a stack from one target. `--force` continues past children with a missing or unreadable install receipt; those directories are reported and left in place, never deleted. |
| `agentstack stack visibility show <STACK\|ORG/STACK> [--team TEAM]` | Show current stack visibility scope. |
| `agentstack stack visibility set <STACK\|ORG/STACK> --scope private\|org\|team [--team TEAM]` | Change stack visibility without changing items. |
| `agentstack stack audit <STACK\|ORG/STACK> [--team TEAM]` | List audit events for one stack. |

Stack install/export resolution fails closed if a child skill is unavailable,
yanked, unapproved, or not visible to the caller.

### Team

| Command | Purpose |
| --- | --- |
| `agentstack team create <ORG/TEAM>` | Create a team. |
| `agentstack team list [--org ORG]` | List teams visible to the caller. Omit `--org` when the active token has one org. |
| `agentstack team inspect <ORG/TEAM>` | Inspect team membership as an org admin or team admin. |
| `agentstack team add-member <ORG/TEAM> <EMAIL> --role member\|team_admin` | Add a team member. |
| `agentstack team set-role <ORG/TEAM> <EMAIL> --role member\|team_admin` | Change a team member role. |
| `agentstack team remove-member <ORG/TEAM> <EMAIL>` | Remove a team member. |

Org admins manage team membership. Team admins can inspect their team and can
manage team-scoped resource lifecycle where their org role allows it; V1 does
not include configurable team permission policies.

### Install

The `install` namespace is the low-level receipt and batch namespace. Normal
single-resource flows should use `skill show --target`, `skill update`,
`skill uninstall`, `stack show --target`, `stack update`, and
`stack uninstall`.

| Command | Purpose |
| --- | --- |
| `agentstack install list [--kind skill\|stack\|all] [--target TARGET]` | List installed receipts. `--kind all --json` returns one object with separate `skills` and `stacks` arrays. |
| `agentstack install why <SKILL> --target <TARGET>` | Explain whether a skill was installed directly or through stack receipts, which stacks require it, and whether direct removal is safe. Registry installs also try a best-effort current-version check. |
| `agentstack install update --all [--target TARGET] [--check] [--force]` | Process direct skill receipts. |
| `agentstack install doctor --target <TARGET>` | Diagnose install locks, staging dirs, receipt parseability, and registry lifecycle of installed versions. |
| `agentstack install unlock --target <TARGET> [--force]` | Remove a stale install lock. |

`install why --json` keeps legacy freshness fields and includes explicit
automation fields:

```json
{
  "skill": "common-review",
  "target": "repo-codex",
  "source_type": "registry",
  "source_ref": "acme/common-review",
  "installed_version": "3",
  "current_version_known": true,
  "current_version": "3",
  "current_registry_version": "3",
  "update_available": false,
  "registry_check_status": "ok",
  "registry_current": { "version": "3", "hash": { "algorithm": "sha256", "hex": "…" } },
  "registry_check_error": null,
  "provenance": "stack",
  "required_by_stacks": ["acme/engineering-default"],
  "installed_by": { "direct": false, "stacks": ["acme/engineering-default"] },
  "direct_remove_safe": false,
  "safe_to_remove": false,
  "reason": "required by stack acme/engineering-default",
  "next_command": "agentstack skill show common-review --target repo-codex",
  "receipt_path": "/abs/path/.agentstack-install.json"
}
```

`registry_check_status` is `local_install` for local/path installs. Registry
checks use `ok`, `unavailable`, `not_found`, `unauthorized`,
`invalid_receipt`, or `unknown`; `current_version` is only populated when
`current_version_known` is true.

Installing `org/skill@version` records the installed version in the receipt.
Direct managed `skill update` does not keep that direct install pinned; it
refreshes to the skill's current approved version. Stack-owned installs follow
the stack policy, so a pinned stack item stays pinned during stack updates.
`install update --all` processes direct skill receipts only. Local/path
installs are reported as `skipped` without requiring registry auth. If a target
only has stack receipts, human output points to the matching `stack update`
command; JSON output remains an empty direct-skill batch.

`skill update` and `install update --all` refuse to overwrite an install whose
files were modified locally (content drift) unless `--force` is set; review the
local edits with `agentstack skill diff <skill> --target <target>` first. In a
batch run the drifted skill fails its row and the remaining rows still proceed.
Receipts without a recorded content hash cannot be checked for drift; the
update warns and proceeds.

`skill update --check` and `install update --all --check` preview an available
update at file level: they download the new archive and report added, removed,
and changed counts, naming up to 20 files per group. JSON adds `changes`
(`added`, `removed`, `changed`, `unchanged`) and `changes_error` when the
preview download or comparison failed. `--check` is read-only and never
refuses a drifted install: it reports drift via `content_drifted` and suggests
the `--force` update command.

When receipts are registry-sourced and the registry is reachable with a valid
token, `install doctor` also runs lifecycle checks with stable codes:
`installed_version_yanked` (fail, with the yank reason and a fix command),
`installed_version_deprecated` (warn), a single `installed_version_outdated`
rollup note suggesting `agentstack install update --all`, and a single
`registry_lifecycle_skipped` note when the registry is unreachable or no token
is active. Lifecycle checks only consult installs recorded against the active
registry; receipts from other registries are skipped with the same note. JSON
reports them in a `lifecycle` array.

For agents reporting deployment drift, prefer the typed stack check:

```sh
agentstack stack update acme/engineering-default --target codex-repo --check --json
```

Summarize `updated`, the stack `target`, and the `added`, `removed`, `changed`,
`pruned`, and `detached` item arrays. `changed` rows include installed and
resolved versions and hashes. The JSON target value remains the stable ID such
as `repo-codex`, even when the command used the alias `codex-repo`.

Every successful `skill install` writes `.agentstack-install.json` inside the
installed skill. Every successful `stack install` also writes a stack receipt
under `<target>/.agentstack-stacks/<org>/<stack>/`. Receipts never contain
tokens. When an explicit repo-scoped or `local` target has no configured
override, registry installs register the default target path only after the
remote install is authorized and succeeds. User-level `codex` and
`claude-code` must be registered first with `agentstack target setup` or
`agentstack target set`.

Registry installs record a package hash and local path installs record an
install-tree hash for provenance, so `skill show --target` and JSON `hash_kind`
label them separately. Both also record a `content_hash` of the installed files
that `skill show --target` and `install doctor` recheck for drift.

Local receipts detect *content* drift via the recorded content hash, but
`installed_by` and the other receipt fields are local and advisory; they are not
cryptographically attested. For authoritative who/when provenance, use the
server-side audit trail (`agentstack audit`, `skill audit`, `stack audit`).

### Sync

| Command | Purpose |
| --- | --- |
| `agentstack sync [--manifest PATH] [--check] [--prune] [--yes]` | Converge install targets to a repo-root `agentstack.toml` manifest. |

The manifest declares `[[stacks]]` and `[[skills]]` entries with `ref` and
`target` fields; skill refs may pin `@VERSION`. Manifest refs must be fully
qualified (`org/name`). Sync installs missing entries and updates outdated or
drifted ones through the normal install pipeline; `--check` reports without
writing. `--prune` removes only receipt-backed installs in manifest-declared
targets that the manifest no longer declares; unmanaged files (including
symlinked directories) are never touched, and the prune pass is skipped
entirely when any manifest entry failed. A missing manifest fails with a structured `manifest_missing` error
that embeds the manifest skeleton. Entry actions are `installed`, `updated`,
`up-to-date`, `would-install`, `would-update`, `pruned`, `would-prune`, and
`failed`; the command exits nonzero when any entry failed. Unlike
single-action commands, a failed sync still emits the full result payload on
stdout (per-entry actions and `prune_skipped` are needed to act on partial
failures) alongside the error envelope on stderr.

### Auth, Registry, Config, Target

| Command | Purpose |
| --- | --- |
| `agentstack auth login [--provider google] [--no-browser] [--callback-port PORT] [--timeout-seconds SECONDS]` | Start browser OAuth for a human user, validate the minted AgentStack token against the active registry, then store it in the credential store. Does not mutate the registry URL. |
| `agentstack auth login --token-stdin` | Raw-token operator fallback: validate one issued AgentStack token from stdin, then store it in the credential store. |
| `agentstack auth logout` | Remove the stored token. |
| `agentstack auth status` | Show local registry URL, token presence, token source, and credential store without calling the registry. |
| `agentstack auth whoami` | Verify the active token. |
| `agentstack registry use <URL>` | Persist a registry URL that overrides the built-in default. |
| `agentstack registry show` | Show the active registry URL and source, including the built-in default when no override is set. |
| `agentstack registry ping` | Check public reachability of the active registry. |
| `agentstack registry ping --auth` | Check reachability and validate the active bearer token. |
| `agentstack config show` | Show raw local config, including target overrides. |
| `agentstack target setup [TARGET] [--path ABS] [--yes]` | Configure a built-in install target. Required before first install into user-level `claude-code` or `codex`; also the supported way to register a non-default path. |
| `agentstack target list` | List known targets and resolved paths. |
| `agentstack target detect` | Diagnose target existence and writability. |
| `agentstack target path <TARGET>` | Print one resolved built-in target path. |
| `agentstack target set <TARGET> --path PATH` | Persist a built-in target path override. |
| `agentstack target unset <TARGET>` | Remove a built-in target path override. |

Target names are not arbitrary labels. The known built-ins are
`claude-code`, `codex`, `repo-claude-code`, `repo-codex`, and `local`.
Natural aliases `claude-code-repo` and `codex-repo` are accepted for the
repo-scoped targets.

Registry URL precedence is `AGENTSTACK_REGISTRY_URL`, then persisted config,
then the built-in default `https://registry.agentstack.gg`.
Token precedence is `AGENTSTACK_TOKEN`, then `AGENTSTACK_TOKEN_PATH`, then the
credential-store token saved by `auth login`. By default the credential store
is `credentials.json` under the AgentStack config directory with file mode
`0600`; set `AGENTSTACK_CREDENTIAL_STORE=keychain` to opt in to the OS keychain
on macOS and Windows. Linux builds use the file store. A failed `auth login`
does not replace an existing stored token.

Use `agentstack auth login` for human users. It starts a browser OAuth flow by
default, with Google as the first provider. `--no-browser` prints the
authorization URL instead of opening it. Browser OAuth listens on
`http://127.0.0.1:49152/auth/callback` by default; `--callback-port PORT`
overrides that loopback port. Piped stdin or `--token-stdin` is the
raw-token operator fallback. Use `AGENTSTACK_TOKEN_PATH` or
`AGENTSTACK_TOKEN` for agents, CI, and headless jobs. Prefer
`AGENTSTACK_TOKEN_PATH` when a secret manager can mount a file; use
`AGENTSTACK_TOKEN` only as a per-process secret variable. Use
`agentstack auth status` to inspect local auth state without a network call. Do
not run `auth login` from automation.

### Audit, Cache, Doctor, Completion

| Command | Purpose |
| --- | --- |
| `agentstack audit list [--org ORG]` | List registry audit events visible to the caller. Omit `--org` when the active token has one org. |
| `agentstack audit show <AUDIT_EVENT_ID> [--org ORG]` | Inspect one audit event. Omit `--org` when the active token has one org. |
| `agentstack cache path` | Print the local package cache path. |
| `agentstack cache list` | List cached skill packages. |
| `agentstack cache remove <SKILL> [--force]` | Remove cached packages for one skill. |
| `agentstack doctor` | Diagnose local config, cache, auth, registry, targets, and receipt counts. A missing token, missing config/cache dirs, and unconfigured user-level `claude-code` / `codex` targets are not first-run failures. Those user-level targets warn only when their default directory already exists. `next:` is only printed for actionable fixes. |
| `agentstack completion <SHELL>` | Print shell completion script. `completion` does not support `--json`. |

Registry resource mutation JSON includes `audit_event_id` for durable operator
tracing. That includes skill push, skill version approve/yank/deprecate,
visibility changes, and stack create/add/remove. Local auth, registry config,
target, cache, and diagnostic flows do not emit audit ids.

## Install and Export Semantics

`skill install` and `stack install` create managed installs and receipts.
Managed installs are the only inputs to `skill show --target`, `skill update`,
`skill uninstall`, `stack show --target`, `stack update`, and `stack uninstall`.

Direct `skill install` refuses to replace an existing managed install unless
`--force` is set. Use `skill update` to refresh registry skills. `stack
install` refuses to adopt an existing direct skill install without `--force`
so stack removal cannot accidentally delete independently installed skills.
Re-running `stack install` for the same stack is treated as a stack-owned
refresh and reports refreshed child skills rather than a foreign overwrite.

A direct install from `org/skill@version` writes that installed version to the
receipt, but `skill update <skill>` tracks the skill's current approved
version. Pinned behavior is a stack policy: stack-owned items pinned in a stack
remain pinned when updating the stack.

Installing or updating into a `claude-code` or `codex` target (including the
repo-scoped variants) applies the skill's `platform/<name>/` files over the
installed skill root after extraction. A platform file replaces the base file
at the same relative path; the `platform/` directory itself is kept verbatim;
`local` gets no overlay. Receipt content hashes reflect the post-overlay
contents. Human output prints `applied platform overlay: ...`; JSON exposes
`overlay` (`{platform, files}` or `null`) on skill install/update and stack
item rows. Installing a registry skill whose platform tags are non-empty and do not
include the target platform prints a warning (suppressed by `--quiet`;
`platform_warning` in JSON); untagged skills never warn.

`skill export` and `stack export` write unmanaged files. They verify metadata,
download archives, verify SHA-256 hashes, and unpack files, but they do not
write install receipts and never apply platform overlays. Use export for
CI/build folders and one-off agent workspaces.

The CLI does not preserve deprecated command or resource aliases. Use the
canonical resource paths above in docs, scripts, generated next commands, and
test fixtures.

## Approval and Visibility

Approval/current movement is separate from visibility/scope:

- `skill push` creates an immutable candidate version.
- `skill version approve` marks exactly one uploaded version as approved/current.
- `skill version yank` and `skill version deprecate` are lifecycle annotations
  on pinned versions.
- `--scope` controls who can read a skill or stack; it does not approve
  or publish a version as current.

`team` scope is supported for trusted teams that need a boundary between
personal/private resources and org-wide resources. It requires `--team <TEAM>`.

## JSON Output

Patch releases may add fields but must not remove or repurpose documented
fields. Errors in JSON mode are written to stderr:

```json
{
  "error": {
    "code": "command_failed",
    "message": "<short>",
    "causes": ["<cause>"],
    "resource": "optional resource ref or path",
    "action": "optional action",
    "status": "optional local status",
    "http_status": 404,
    "machine_hint": "optional automation guidance",
    "auth_methods": ["optional supported auth method"],
    "next_command": "agentstack skill search code-review --org acme",
    "next_command_template": "agentstack skill search <query>"
  }
}
```

`next_command` is always intended to be runnable as-is. Placeholder guidance
uses `next_command_template` instead, so agents do not need to parse or reject
angle-bracket tokens.

Unauthenticated errors may include `machine_hint` and `auth_methods` so headless
callers can select `AGENTSTACK_TOKEN_PATH` or `AGENTSTACK_TOKEN` without parsing
human prose.

Registry resource mutation success includes `audit_event_id`. Local config,
auth, target, cache, and diagnostic commands do not emit audit ids.

The keys below describe successful JSON payloads. Command failures emit the
standard `{"error": ...}` envelope to stderr; they do not also emit a partial
success payload on stdout.

| Command family | Required top-level keys |
| --- | --- |
| `skill init` | `name`, `path`, `skill_md`, `subdirs` |
| `skill validate` | `ok`, `path`, `name`, `description`, `errors` |
| `skill lint` | `ok`, `path`, `validation_errors`, `warnings` |
| `skill inspect` | `name`, `description`, `path`, `skill_md`, `directories`, `unknown_files`, `errors`, `warnings`, `package_hash` |
| `skill security-scan` | `ok`, `path`, `scanned_files`, `skipped_binary_files`, `validation_errors`, `findings`, `summary` |
| `skill scan` | `skills`, plus `empty_message` and concrete `next_command` when empty |
| `skill pack` | `name`, `version`, `path`, `files`, `size_bytes`, `sha256`, `cached_at`, `lint_warnings`, `lint_next_command`, `next_command` |
| `skill unpack` | `name`, `out`, `files`, `sha256` |
| `skill push` | `skill_ref`, `version`, `sha256`, `visibility`, `metadata`, `lint_warnings`, `url`, `audit_event_id`, `next_commands`; dry-run uses `would_upload`, `authorization_checked`, `metadata`, `skill_ref`, `version`, `sha256`, `visibility`, `lint_warnings`, `size_bytes` |
| `skill push --all` | `batch`, `dry_run`, `org`, `path`, `pushed`, `skipped`, `failed`, `summary` (`summary.would_push` appears on dry-runs) |
| `skill adopt` | `dry_run`, `org`, `path`, `adopted` (rows include `name`, `skill_ref`, `version`, `audit_event_id`), `skipped` (rows include `path`, `reason`), `failed` (rows include `name`, `reason`), `summary` (`summary.would_adopt` appears on dry-runs), plus `empty_message` when no skills were found |
| `skill candidates` | `candidates` (rows include `org`, `skill`, `version`, `approve_command`, plus `created_at` and `owner` when present), `scanned_skills`, `truncated`, plus `empty_message` and a concrete `next_command` when empty |
| `skill version approve` | `skill_ref`, `metadata`, `audit_event_id`, `next_commands`, `next_command_templates` |
| `skill version yank/deprecate` | `skill_ref`, `action`, `metadata`, `audit_event_id`, `next_commands` |
| `skill list` | `org`, `filters`, `skills`, plus `empty_message` and a concrete `next_command` or `next_command_template` when empty |
| `skill search` | `query`, `filters`, `results`, plus `empty_message` and a concrete `next_command` when empty |
| `skill show/status/impact/audit/visibility` | `metadata`, `status` includes `skill`, `versions`, and optional `next_command` or `next_command_template`; impact includes `skill`, `summary`, `used_by`, and `next_commands`; audit includes `events`; visibility-specific fields otherwise |
| `skill version list/show` | `skill_ref`, `versions` plus `empty_message` and a concrete `next_command` when empty, or `version` |
| `skill diff` | `left`, `right`, `added`, `removed`, `changed`, `unchanged_count`, `changed_count`, `is_empty` |
| `skill install` | `name`, `installed_as`, `target`, `destination`, `source_type`, `source_ref`, `registry_url`, `org`, `version`, `hash`, `hash_kind`, `receipt`, `overlay`, `platform_warning`, `warnings`, `next_commands` |
| `skill show --target` | `receipt`, `receipt_path`, `validation`, `hash_kind` |
| `skill update` | `skill_name`, `target`, `source_ref`, `registry_url`, `installed_version`, `latest_version`, `update_available`, `installed_yanked`, optional `installed_yank_reason`, `updated`, `forced`, `content_drifted`, `destination`, `receipt`, `cache_package`, `overlay`, `next_command`; `--check` adds `changes` (`added`, `removed`, `changed`, `unchanged`) and `changes_error` when the file preview was unavailable |
| `skill uninstall` | `removed` on apply or `would_remove` plus `dry_run` on dry-run; `source_type`, `source_ref`, `version`, `hash` |
| `skill export` | `skill_ref`, `metadata`, `destination`, `next_commands`, `next_command_templates` |
| `stack create/add/remove/show/status/audit/visibility` | `stack`; status includes `next_command`; mutations expose `audit_event_id` in or alongside `stack`; `stack remove --dry-run` emits `dry_run`, `stack`, `would_remove`, `items_after` |
| `stack list` | `org`, `filters`, `stacks`, plus `empty_message` and a concrete `next_command` when empty |
| `stack resolve` | `stack`, `resolved_at`, `manifest_hash`, `items` |
| `stack install` | `org`, `stack`, `target`, `manifest_hash`, `resolved_at`, `stack_receipt`, `items`, `next_commands` |
| `stack show --target` | `receipt`, `receipt_path` |
| `stack update` | `kind`, `org`, `stack`, `target`, `registry_url`, `check`, `force`, `prune`, `updated`, `manifest_hash`, `stack_receipt`, `added`, `removed`, `changed`, `unchanged`, `pruned`, `detached`, `next_command` |
| `stack uninstall` | `kind`, `org`, `stack`, `target`, `receipt`, `dry_run`, `items`, `summary`; item rows include `skill`, `path`, `action`, optional `reason`; summary includes `removed`, `kept_shared`, `kept_foreign`, `left_in_place`, `missing` |
| `stack export` | `stack`, `manifest_hash`, `destination_parent`, `items`, `next_command_templates` |
| `team create/add-member/set-role/remove-member` | `team`, `audit_event_id` |
| `team list/inspect` | `teams` plus `empty_message` and `next_command_template` when empty, or `team` |
| `install list` | `installed` rows include `hash_kind`, plus `empty_message` and optional concrete `next_command` or `next_command_template` when empty; `--kind all` emits `skills`, `stacks`, and optional `empty_message` |
| `install why` | `skill`, `target`, `source_type`, `source_ref`, `installed_version`, `current_version_known`, `current_version`, `current_registry_version`, `update_available`, `registry_check_status`, `registry_current`, `registry_check_error`, `provenance`, `required_by_stacks`, `installed_by`, `direct_remove_safe`, `safe_to_remove`, `reason`, `next_command`, `receipt_path` |
| `install update --all` | `batch`, `target`, `check`, `force`, `results`, `summary`; result rows include `skill_name`, `target`, `status`, optional version/yank/force/error fields, and `changes` on `--check` rows with an available update; summary includes `updated`, `already_current`, `update_available`, `skipped`, `failed` |
| `install doctor` | `target`, `target_root`, `lock`, `staging_dirs`, `receipts_parseable`, `receipts_unreadable`, `recorded_package_matches`, `drifted`, `unknown`, `lifecycle` |
| `sync` | `kind`, `manifest`, `check`, `prune`, optional `prune_skipped` (reason the prune pass did not run, e.g. failed entries), `entries` (rows include `ref`, `target`, `kind`, `action`, optional `version` and `detail`), `summary` (`installed`, `updated`, `up_to_date`, `would_install`, `would_update`, `pruned`, `would_prune`, `failed`) |
| `auth login/logout/status/whoami` | `logged_in` or action-specific status; login and local status JSON include optional `next_command` or `next_command_template`, and never token material |
| `registry show/use/ping` | `url`, `source` or action-specific status; ping JSON includes `ok`, `authenticated` (`null` unless `--auth` checked the token, then `true`/`false`), optional `email`, `server_version`, and `next_command` when auth was not checked |
| `config show` | `path`, `config` |
| `cache list/remove` | `entries` plus `empty_message` and concrete `next_command` when empty, or `name`, `removed`, `root`, `skills_dir` |
| `target setup/list/detect/path` | target-specific path and status fields; setup/detect include concrete `next_commands` or placeholder `next_command_templates` when fixes are available |
| `audit list/show` | `events` plus optional `next_command_template`, or `event` |
| `doctor` | `cli_version`, `checks`, `summary` |

`config path` and `cache path` are path printers. They may accept the global
`--json` flag for CLI consistency, but their contract is plain path text on
stdout. `target path --json` emits `target`, `path`, and `source`.

## Environment Variables

| Variable | Purpose |
| --- | --- |
| `AGENTSTACK_CONFIG_DIR` | Override `~/.agentstack`. |
| `AGENTSTACK_CACHE_DIR` | Override the package cache directory. |
| `AGENTSTACK_REGISTRY_URL` | Override the active registry URL for remote commands. |
| `AGENTSTACK_TOKEN` | Override the stored token for one process. |
| `AGENTSTACK_TOKEN_PATH` | Read one bearer token from a mounted secret file. Read-only; never written by AgentStack. |
| `AGENTSTACK_TOKEN_FILE` | Test-only plaintext token store; never use in production. |
| `AGENTSTACK_ALLOW_TOKEN_FILE` | Explicit opt-in for `AGENTSTACK_TOKEN_FILE` outside debug builds. |
| `AGENTSTACK_NONINTERACTIVE` | Disable prompts when set to any non-empty value except `0`, `false`, `no`, or `off`. |
| `CI` | Disable prompts when set to any non-empty value except `0`, `false`, `no`, or `off`. |

## Exit Codes

| Code | Meaning |
| --- | --- |
| `0` | Success. |
| `1` | Runtime failure. |
| `2` | Argument parsing failure. |
