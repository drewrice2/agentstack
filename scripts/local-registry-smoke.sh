#!/usr/bin/env bash
set +x
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

CLI="${AGENTSTACK_CLI:-agentstack}"
if ! command -v "$CLI" >/dev/null 2>&1; then
  echo "agentstack CLI not found: $CLI" >&2
  exit 1
fi

SMOKE_ROOT="$(mktemp -d)"
declare +x TOKEN
TOKEN=""

export AGENTSTACK_CONFIG_DIR="$SMOKE_ROOT/config"
export AGENTSTACK_CACHE_DIR="$SMOKE_ROOT/cache"
export AGENTSTACK_CREDENTIAL_STORE=file
unset AGENTSTACK_REGISTRY_URL AGENTSTACK_TOKEN AGENTSTACK_TOKEN_PATH \
  AGENTSTACK_TOKEN_FILE AGENTSTACK_ALLOW_TOKEN_FILE

cleanup() {
  "$CLI" --json --no-input auth logout >/dev/null 2>&1 || true
  TOKEN=
  if [[ -n "${SMOKE_ROOT:-}" && -d "$SMOKE_ROOT" ]]; then
    rm -rf -- "$SMOKE_ROOT"
  fi
}
trap cleanup EXIT

mkdir -p "$SMOKE_ROOT/local-registry-smoke"
cat >"$SMOKE_ROOT/local-registry-smoke/SKILL.md" <<'EOF'
---
name: local-registry-smoke
description: Use when verifying a local AgentStack registry through the public CLI.
---

# Local registry smoke

Use this skill to verify a local AgentStack registry through the public CLI.
EOF

TOKEN="$("$ROOT/scripts/local-up.sh")"

"$CLI" --json --no-input registry use http://127.0.0.1:8080 >/dev/null
printf '%s' "$TOKEN" | "$CLI" --json --no-input auth login --token-stdin >/dev/null
TOKEN=

json_matches() {
  # macOS Bash 3.2 treats quoted `=~` patterns as literals.
  printf '%s\n' "$1" | grep -Eq -- "$2"
}

ping_json="$("$CLI" --json --no-input registry ping --auth)"
if ! json_matches "$ping_json" '"authenticated"[[:space:]]*:[[:space:]]*true'; then
  echo "registry ping did not report authenticated=true" >&2
  printf '%s\n' "$ping_json" >&2
  exit 1
fi

push_json="$("$CLI" --json --no-input skill push \
  "$SMOKE_ROOT/local-registry-smoke" \
  --org local \
  --scope org \
  --yes)"
if ! json_matches "$push_json" '"visibility"[[:space:]]*:[[:space:]]*"org"'; then
  echo "skill push did not report org visibility" >&2
  printf '%s\n' "$push_json" >&2
  exit 1
fi
if ! json_matches "$push_json" 'local/local-registry-smoke@'; then
  echo "skill push did not report the local/local-registry-smoke skill ref" >&2
  printf '%s\n' "$push_json" >&2
  exit 1
fi

echo "local registry smoke passed" >&2
