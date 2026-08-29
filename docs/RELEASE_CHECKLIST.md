# Release checklist

Run from the repository root on the host architecture you intend to ship.

## 1. Record inputs

```sh
BASELINE_SHA="$(git rev-parse HEAD)"
VERSION="$(cargo metadata --no-deps --format-version 1 \
  | sed -n 's/.*"name":"agentstack","version":"\([^"]*\)".*/\1/p' \
  | head -1)"
HOST="$(rustc -vV | awk '/^host:/ { print $2 }')"

test -n "$VERSION"
test -n "$HOST"
printf 'baseline_sha=%s\nversion=%s\nhost=%s\n' "$BASELINE_SHA" "$VERSION" "$HOST"
```

## 2. Confirm the tree is clean

```sh
git status --short --branch
```

Expected: no modified, staged, or untracked source files. Generated `dist/`
artifacts are okay only after the build step.

## 3. Run checks

```sh
cargo fmt --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
bash -n scripts/build-cli-local.sh
git diff --check
```

## 4. Scan for token leakage

Investigate every hit before shipping an artifact.

```sh
git grep -n -I -E \
  '(AGENTSTACK_TOKEN=.{12,}|Bearer [A-Za-z0-9._~+/=-]{20,}|token: [A-Za-z0-9._~+/=-]{20,}|sk-[A-Za-z0-9_-]{20,}|gh[pousr]_[A-Za-z0-9_]{20,}|github_pat_[A-Za-z0-9_]{20,}|xox[baprs]-[A-Za-z0-9-]{20,}|AKIA[0-9A-Z]{12,}|AIza[0-9A-Za-z_-]{20,}|eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}|(SECRET|PASSWORD|API_KEY|TOKEN)=.{12,}|BEGIN (RSA|OPENSSH|PRIVATE) KEY)' \
  -- . ':!docs/RELEASE_CHECKLIST.md'
```

## 5. Build the artifact

```sh
bash scripts/build-cli-local.sh
```

## 6. Smoke the binary help surface

```sh
DIST_DIR="dist/agentstack-$VERSION-$HOST"
"$DIST_DIR/agentstack" --help >/dev/null
"$DIST_DIR/agentstack" skill --help >/dev/null
"$DIST_DIR/agentstack" stack --help >/dev/null
"$DIST_DIR/agentstack" target --help >/dev/null
"$DIST_DIR/agentstack" audit --help >/dev/null
"$DIST_DIR/agentstack" doctor --help >/dev/null
```

## 7. Do not ship

- `.env`, `.env.*`, `credentials.json`, API keys, bearer tokens, or token files
- `AGENTSTACK_CONFIG_DIR`, `AGENTSTACK_CACHE_DIR`, or `AGENTSTACK_TOKEN_FILE`
  contents
- registry databases, blob directories, or local research captures
- private customer skills or generated work directories
- `target/`, local build caches, or unrelated repo worktrees

## 8. Record the release SHA

```sh
RELEASE_SHA="$(git rev-parse HEAD)"
printf 'agentstack %s %s\n' "$VERSION" "$RELEASE_SHA" > "dist/agentstack-$VERSION-$HOST/RELEASE.txt"
git status --short --branch
```

Do not include tokens in the release note.

## 9. Tag

Tag only after the release SHA is final, through the repository's normal
release process.
