#!/usr/bin/env bash
set +x
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 1
  fi
}

need docker
need curl

docker compose version >&2

compose() {
  docker compose --project-directory "$ROOT" -f "$ROOT/docker-compose.yml" "$@"
}

echo "building agentstack-server image" >&2
compose build agentstack-server >&2

echo "starting postgres" >&2
compose up -d postgres >&2

postgres_ready=0
postgres_deadline=$((SECONDS + 60))
while (( SECONDS < postgres_deadline )); do
  if compose exec -T postgres pg_isready -U postgres -d agentstack >/dev/null 2>&1; then
    postgres_ready=1
    break
  fi
  sleep 1
done

if (( postgres_ready == 0 )); then
  echo "postgres did not become ready within 60 seconds" >&2
  compose logs postgres >&2 || true
  exit 1
fi

echo "initializing database" >&2
compose run --rm --no-deps -T agentstack-server init-db >&2

echo "ensuring local organization exists" >&2
set +e
org_output="$(compose run --rm --no-deps -T agentstack-server \
  org create local \
  --name Local \
  --owner-email operator@example.com \
  --owner-name "Local Operator" 2>&1)"
org_status=$?
set -e

if (( org_status == 0 )) || [[ "$org_output" == *'org `local` already exists'* ]]; then
  if [[ -n "$org_output" ]]; then
    printf '%s\n' "$org_output" >&2
  fi
else
  printf '%s\n' "$org_output" >&2
  exit "$org_status"
fi

echo "starting agentstack-server" >&2
compose up -d --no-deps agentstack-server >&2

server_healthy=0
server_deadline=$((SECONDS + 60))
while (( SECONDS < server_deadline )); do
  if curl -fsS http://127.0.0.1:8080/healthz >/dev/null 2>&1; then
    server_healthy=1
    break
  fi
  sleep 1
done

if (( server_healthy == 0 )); then
  echo "agentstack-server did not become healthy within 60 seconds" >&2
  compose logs agentstack-server >&2 || true
  exit 1
fi

echo "issuing 30-day local token" >&2
declare +x TOKEN TOKEN_OUTPUT TOKEN_SENTINEL
TOKEN_SENTINEL=$'\001'
set +e
TOKEN_OUTPUT="$(
  compose run --rm --no-deps -T agentstack-server \
    token issue operator@example.com \
    --label local-up \
    --expires-in-days 30 \
    --raw
  token_command_status=$?
  printf '%s' "$TOKEN_SENTINEL"
  exit "$token_command_status"
)"
token_status=$?
set -e

if (( token_status != 0 )); then
  echo "failed to issue local token" >&2
  exit "$token_status"
fi
if [[ "$TOKEN_OUTPUT" != *"$TOKEN_SENTINEL" ]]; then
  echo "server token output was not captured" >&2
  exit 1
fi
TOKEN_OUTPUT="${TOKEN_OUTPUT%$'\001'}"
if [[ "$TOKEN_OUTPUT" != *$'\n' ]]; then
  echo "server returned token output without a newline" >&2
  exit 1
fi
TOKEN="${TOKEN_OUTPUT%$'\n'}"

if [[ -z "$TOKEN" ]]; then
  echo "server returned an empty token" >&2
  exit 1
fi
if [[ "$TOKEN" == *$'\r'* ]]; then
  echo "server returned a token containing carriage return" >&2
  exit 1
fi
if [[ "$TOKEN" == *$'\n'* ]]; then
  echo "server returned multiline token output" >&2
  exit 1
fi

cat >&2 <<'EOF'

Local AgentStack registry is ready. Configure the public CLI with:
agentstack registry use http://127.0.0.1:8080
read -r -s -p 'Token: ' TOKEN; printf '\n'
printf '%s' "$TOKEN" | agentstack auth login --token-stdin
agentstack registry ping --auth
agentstack skill push ./my-skill --org local --scope org
EOF

printf '%s\n' "$TOKEN"
