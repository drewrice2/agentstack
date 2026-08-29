# Install, update, and inspect managed context

Read this when installing, updating, removing, exporting, or explaining
AgentStack-managed context. Installs are local; only pulling a registry skill or
stack needs a token.

## Targets

Targets are built-in install destinations. The same skill installs into either
runtime — only the target differs:

| Target | Default path |
| --- | --- |
| `claude-code` | `~/.claude/skills/` |
| `codex` | `~/.codex/skills/` |
| `repo-claude-code` | `<repo>/.claude/skills/` |
| `repo-codex` | `<repo>/.codex/skills/` |
| `local` | `~/.agentstack/skills/` |

Resolve paths instead of guessing:

```bash
agentstack target list
agentstack target detect
agentstack target path codex
```

Installing into a `claude-code` or `codex` target (including `repo-`
variants) applies the skill's matching `platform/<name>/` files over the
installed root — the output says `applied platform overlay`. Exports stay
verbatim.

User-level `codex` and `claude-code` require setup before first install.
Repo-scoped targets and `local` may auto-register their default path on first
successful install when no override exists.

```bash
agentstack target setup codex --yes
agentstack target setup codex --path /absolute/path/to/skills --yes
agentstack target set codex --path /absolute/path/to/skills
agentstack target unset codex
```

`codex-repo` and `claude-code-repo` are accepted aliases for the `repo-` targets.

## No-token learning loop

A first successful loop using the bundled example skill, no registry required:

```bash
agentstack skill inspect examples/skills/agentstack
agentstack skill validate examples/skills/agentstack
agentstack target setup local --yes
agentstack skill install examples/skills/agentstack --target local
agentstack skill show agentstack --target local
```

This demonstrates skill shape, target setup, managed receipts, and the
update/remove lifecycle without touching hosted state.

## Managed install vs export

Managed install writes receipts:

```bash
agentstack skill install <org>/<skill> --target codex
agentstack stack install <org>/<stack> --target codex
agentstack install list --kind all --target codex --json
```

Receipts record source, version, hash, target, destination, and stack
provenance. They never contain tokens. Receipts power:

```bash
agentstack skill show <skill> --target codex --json
agentstack install why <skill> --target codex --json
agentstack skill update <skill> --target codex --check
agentstack skill uninstall <skill> --target codex --dry-run
```

Export writes unmanaged files (CI, build folders, one-off snapshots):

```bash
agentstack stack export <org>/<stack> --out ./skills --dry-run --json
agentstack skill export <org>/<skill>@<version> --out ./skills
```

Do not expect `skill update` or `skill uninstall` to manage exported files.

## Converge a repo with sync

A repo-root `agentstack.toml` declares which stacks and skills belong in which
targets; `agentstack sync` makes the targets match it — installing what is
missing and updating what is outdated or locally modified:

```toml
[[stacks]]
ref = "<org>/<stack>"
target = "repo-claude-code"

[[skills]]
ref = "<org>/<skill>"        # pin with "<org>/<skill>@<version>"
target = "repo-codex"
```

```bash
agentstack sync --check
agentstack sync --yes
```

`--prune` additionally removes receipt-backed installs the manifest no longer
declares; unmanaged files are never touched, and the prune pass is skipped when
any entry failed. Always show the user `--check` output before applying.

## Update safely

Inspect first, apply only after agreement:

```bash
agentstack skill show <skill> --target <target> --json
agentstack skill update <skill> --target <target> --check
agentstack skill update <skill> --target <target>
```

`--check` previews the version delta and a file-level added/removed/changed
summary. For the full content diff of an installed copy against the registry,
use `agentstack skill diff <skill> --target <target>`.

For stacks, add `--prune` only when the user wants obsolete children removed:

```bash
agentstack stack show <org>/<stack> --target <target> --json
agentstack stack update <org>/<stack> --target <target> --check
agentstack stack update <org>/<stack> --target <target>
```

Stop before writing when the receipt is local/path, installed files differ from
receipt validation, the user is editing the install, or the skill is owned by a
stack. To refresh every direct skill receipt at once, use
`agentstack install update --all --check` first, then without `--check`.

## Remove safely

Explain provenance first:

```bash
agentstack install why <skill> --target <target>
agentstack skill uninstall <skill> --target <target> --dry-run
```

If the skill is stack-owned, remove or update the stack instead of the skill:

```bash
agentstack stack uninstall <org>/<stack> --target <target> --dry-run
agentstack stack uninstall <org>/<stack> --target <target>
```

Do not remove shared children unless dry-run says they are no longer needed.

## Interpreting receipts

`installed: []` means AgentStack has no managed receipts for that query. It does
not prove the filesystem has no hand-copied skills or unmanaged exports.

Useful summary: "For target `<target>`, AgentStack manages `<stack-count>`
stack install(s), `<direct-count>` direct skill install(s), and `<local-count>`
local/path install(s); unmanaged exports or copied files are outside these
receipts."
