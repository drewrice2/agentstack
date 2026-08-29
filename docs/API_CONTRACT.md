# AgentStack Registry API Contract

This is the canonical wire contract for the AgentStack registry API. REST
collection paths stay plural even though CLI namespaces are singular.

## Status

Pre-1.0. Payloads may gain additive fields. Existing `/v1` fields and error
codes should remain stable inside a compatible release.

## Authentication

`GET /v1/ping` is public. Browser-login bootstrap endpoints
`POST /v1/auth/oauth/start` and `POST /v1/auth/oauth/token` are public because
they create a new human CLI session. Every other `/v1` endpoint requires:

```text
Authorization: Bearer <token>
```

Tokens must never appear in ordinary JSON responses, human output, logs, or
error messages. The OAuth token endpoint is the only exception: it returns one
new AgentStack bearer token in its TLS response body as `access_token`. The
server must not log, audit, or echo that raw token, and the CLI must validate it
with `/v1/whoami` before storing it. CLI environment variables such as
`AGENTSTACK_TOKEN` and `AGENTSTACK_REGISTRY_URL` are client-side resolution
rules, not registry API features.

## Health

`GET /healthz` is public and unauthenticated:

```json
{
  "status": "ok",
  "server_version": "0.1.0",
  "build": {
    "git_sha": "9ea98512bba8d80406e280546f2c3916d5b8ed93",
    "image_ref": "agentstack-server:9ea98512bba8d80406e280546f2c3916d5b8ed93-amd64",
    "image_tag": "9ea98512bba8d80406e280546f2c3916d5b8ed93-amd64"
  }
}
```

`GET /v1/ping` returns the same public shape. The `build` object is included
for deployment provenance; individual fields are optional and may be omitted
when the server has no corresponding build environment value.

## Roles

| Role | Scope |
| --- | --- |
| `server_admin` | Global server-admin authority via `users.is_server_admin`. |
| `org_admin` | Manage users, tokens, roles, skills, stacks, and approvals within one org. |
| `publisher` | Push candidate skill versions within one org. |
| `reader` | List, search, export, install, and inspect approved versions they can see. |

Teams add `member` and `team_admin` membership inside an org. Org admins create teams
and manage membership. Team members can read team-visible approved resources in
their team. For team-scoped skills, team admins can approve versions, yank and
deprecate, and change visibility; they can also create and mutate team-scoped
stacks. Approval, lifecycle, and visibility for org- and private-scoped skills
stay org_admin-only. Do not add nested orgs, approval chains, custom policies,
or broad configurable RBAC in V1.

## Browser OAuth Login

The CLI implements OAuth as a browser login to the AgentStack registry, not as
direct Google API access. Provider tokens stay server-side. The CLI stores only
the AgentStack bearer token minted by the registry.

The first provider is `google`. The CLI:

1. Binds a loopback callback on `127.0.0.1`.
2. Generates PKCE S256 verifier/challenge material and a random state value.
3. Calls `POST /v1/auth/oauth/start` without bearer auth.
4. Opens the returned authorization URL, or prints it when `--no-browser` is
   used. The URL must be either same-origin with the registry or the expected
   provider origin for the selected provider.
5. Accepts exactly one callback at `/auth/callback` and verifies `state`.
6. Calls `POST /v1/auth/oauth/token` without bearer auth.
7. Validates the returned AgentStack token with `GET /v1/whoami` before writing
   it to the CLI credential store.

Start request:

```json
{
  "provider": "google",
  "redirect_uri": "http://127.0.0.1:49152/auth/callback",
  "code_challenge": "<base64url-sha256>",
  "code_challenge_method": "S256",
  "state": "<base64url-random>",
  "client": "agentstack-cli",
  "cli_version": "0.1.0"
}
```

Start response:

```json
{
  "authorization_url": "https://registry.agentstack.gg/v1/auth/oauth/google/authorize?flow=<opaque>",
  "state": "<base64url-random>",
  "expires_in_seconds": 600
}
```

`authorization_url` must be same-origin with the active registry URL or, for
Google, `https://accounts.google.com`. The callback to the CLI must include an
ephemeral AgentStack login code and state:

```text
GET http://127.0.0.1:49152/auth/callback?code=<code>&state=<state>
```

Token exchange request:

```json
{
  "grant_type": "authorization_code",
  "provider": "google",
  "code": "<code>",
  "state": "<state>",
  "redirect_uri": "http://127.0.0.1:49152/auth/callback",
  "code_verifier": "<pkce-verifier>"
}
```

Token exchange response:

```json
{
  "token_type": "Bearer",
  "access_token": "<agentstack-token>",
  "expires_at": "2026-06-04T18:00:00Z",
  "identity": {
    "user": "developer@example.com",
    "email": "developer@example.com",
    "name": "Example User"
  }
}
```

`identity` is advisory. The CLI still validates `access_token` through
`/v1/whoami` before saving it. Stable OAuth error codes should use the normal
error envelope; expected codes include `oauth_denied`, `oauth_expired`,
`oauth_invalid_grant`, and `invite_required`.

## URLs

| Method | Path | Auth | Purpose |
| --- | --- | --- | --- |
| `GET` | `/v1/ping` | public | Reachability and version check. |
| `POST` | `/v1/auth/oauth/start` | public | Start browser OAuth for a human CLI login. |
| `POST` | `/v1/auth/oauth/token` | public | Exchange one browser callback code plus PKCE verifier for an AgentStack bearer token. |
| `GET` | `/v1/whoami` | Bearer | Identity attached to active token. |
| `POST` | `/v1/orgs/{org}/skills` | Bearer | Upload one append-only candidate skill version. |
| `GET` | `/v1/skills` | Bearer | List visible skills across orgs with optional filters. |
| `GET` | `/v1/orgs/{org}/skills` | Bearer | List visible skills in one org. |
| `GET` | `/v1/search` | Bearer | Search visible skills. |
| `GET` | `/v1/orgs/{org}/skills/{name}` | Bearer | Current approved skill metadata. |
| `GET` | `/v1/orgs/{org}/skills/{name}/status` | Bearer | Skill lifecycle status summary. |
| `GET` | `/v1/orgs/{org}/skills/{name}/impact` | Bearer | Visible stack usage for one skill. |
| `GET` | `/v1/orgs/{org}/skills/{name}/audit` | Bearer | Skill-scoped audit events. |
| `GET` | `/v1/orgs/{org}/skills/{name}/visibility` | Bearer | Read skill visibility without moving current. |
| `PATCH` | `/v1/orgs/{org}/skills/{name}/visibility` | Bearer | Change skill visibility only. |
| `GET` | `/v1/orgs/{org}/skills/{name}/versions` | Bearer | List visible uploaded versions. |
| `GET` | `/v1/orgs/{org}/skills/{name}/versions/{version}` | Bearer | Pinned visible version metadata. |
| `GET` | `/v1/orgs/{org}/skills/{name}/versions/{version}/archive` | Bearer | Download archive bytes. |
| `POST` | `/v1/orgs/{org}/skills/{name}/versions/{version}/approve` | Bearer | Approve one version and move current. |
| `POST` | `/v1/orgs/{org}/skills/{name}/versions/{version}/yank` | Bearer | Mark one version unsafe for default install/export. |
| `POST` | `/v1/orgs/{org}/skills/{name}/versions/{version}/deprecate` | Bearer | Mark one version superseded but still available. |
| `POST` | `/v1/orgs/{org}/stacks` | Bearer | Create a stack. |
| `GET` | `/v1/orgs/{org}/stacks` | Bearer | List visible stacks. |
| `GET` | `/v1/orgs/{org}/stacks/{stack}` | Bearer | Inspect one stack and its items. |
| `GET` | `/v1/orgs/{org}/stacks/{stack}/status` | Bearer | Stack lifecycle status summary. |
| `GET` | `/v1/orgs/{org}/stacks/{stack}/audit` | Bearer | Stack-scoped audit events. |
| `PATCH` | `/v1/orgs/{org}/stacks/{stack}` | Bearer | Update stack metadata such as visibility. |
| `PATCH` | `/v1/orgs/{org}/stacks/{stack}/visibility` | Bearer | Change stack visibility only. |
| `POST` | `/v1/orgs/{org}/stacks/{stack}/items` | Bearer | Add or update a stack item. |
| `DELETE` | `/v1/orgs/{org}/stacks/{stack}/items/{skill}` | Bearer | Remove a stack item. |
| `GET` | `/v1/orgs/{org}/stacks/{stack}/resolve` | Bearer | Resolve a stack to concrete approved skill versions. |
| `GET` | `/v1/orgs/{org}/audit` | Bearer | List org audit events visible to the caller. |
| `GET` | `/v1/orgs/{org}/audit/{event_id}` | Bearer | Inspect one org audit event. |
| `POST` | `/v1/orgs/{org}/teams` | Bearer | Create a team. |
| `GET` | `/v1/orgs/{org}/teams` | Bearer | List teams. |
| `GET` | `/v1/orgs/{org}/teams/{team}` | Bearer | Inspect team members. |
| `PUT` | `/v1/orgs/{org}/teams/{team}/members/{email}` | Bearer | Add a member. |
| `PATCH` | `/v1/orgs/{org}/teams/{team}/members/{email}` | Bearer | Change a member role. |
| `DELETE` | `/v1/orgs/{org}/teams/{team}/members/{email}` | Bearer | Remove a member. |

No endpoint redirects. The CLI client does not follow HTTP redirects and
treats any `3xx` response as an error.

## Skill References

- `org/name` means the current approved version.
- `org/name@version` means a pinned version.

These are canonical registry refs. The HTTP API remains org-scoped under
`/v1/orgs/{org}/...`; any CLI support for bare `name` or `name@version`
resolves the org from the active token before calling the API.

The current registry assigns monotonically increasing integer version strings
per skill. V1 does not require SemVer.

## Visibility

| Value | Meaning |
| --- | --- |
| `private` | Original publisher/owner, org admin, or server admin can read. |
| `org` | Everyone in the owning org can read. |
| `team` | Members of the owning team, org admins, server admins, and the owner can read. |

Approval/current movement is not encoded in visibility.

## Skill Metadata

```json
{
  "name": "code-review",
  "description": "Use when reviewing pull requests",
  "org": "acme",
  "visibility": "org",
  "version": "1",
  "hash": { "algorithm": "sha256", "hex": "<64 hex chars>" },
  "platform_tags": ["codex"],
  "created_at": "2026-05-06T17:42:11Z",
  "updated_at": "2026-05-06T17:42:11Z",
  "status": "approved",
  "current": true
}
```

`status` is `candidate`, `approved`, or reserved future value `rejected`.
`current` marks the single current approved version. Lifecycle annotations
such as `yanked_at`, `yank_reason`, `deprecated_at`, and
`deprecation_reason` are omitted when unset.

## Push

`POST /v1/orgs/{org}/skills` is `multipart/form-data` with:

- `metadata`: `application/json` skill metadata. Client-supplied
  `created_at`, `updated_at`, and authoritative `version` are rejected or
  ignored; the registry assigns them.
- `archive`: `application/gzip` deterministic archive bytes.

The registry recomputes SHA-256 and rejects hash mismatches. Accepted pushes
append a candidate version and do not change current.

Response:

```json
{
  "metadata": { "name": "code-review", "org": "acme", "version": "1" },
  "skill_ref": "acme/code-review@1",
  "version": "1",
  "sha256": "<64 hex chars>",
  "visibility": "org",
  "url": null,
  "audit_event_id": "evt_123"
}
```

## Approval and Lifecycle

Approve, yank, and deprecate endpoints return updated skill metadata with an
`audit_event_id` field. The response is the metadata shape, not the push
envelope:

```json
{
  "name": "code-review",
  "description": "Use when reviewing pull requests",
  "org": "acme",
  "owner_email": "owner@example.com",
  "visibility": "org",
  "version": "1",
  "hash": { "algorithm": "sha256", "hex": "<64 hex chars>" },
  "platform_tags": ["codex"],
  "created_at": "2026-05-06T17:42:11Z",
  "updated_at": "2026-05-06T17:45:11Z",
  "status": "approved",
  "current": true,
  "audit_event_id": "evt_124"
}
```

`approve` marks the version `approved` and makes it current. It does not
change visibility.

`yank` and `deprecate` require:

```json
{ "reason": "bad archive" }
```

Yanked versions are hidden from discovery and refused for default install or
export. Deprecated versions remain available by default.

## Export/Install Metadata Flow

Current metadata:

`GET /v1/orgs/{org}/skills/{name}`

Pinned metadata:

`GET /v1/orgs/{org}/skills/{name}/versions/{version}`

Archive:

`GET /v1/orgs/{org}/skills/{name}/versions/{version}/archive`

The archive response uses `Content-Type: application/gzip` and
`x-agentstack-sha256: <64 hex chars>`. The CLI re-hashes archive bytes before
writing files.

If a skill has candidate uploads but no current approved version, current
metadata returns `409 no_current_version`.

## List/Search

`GET /v1/search?q=...` returns:

```json
{
  "results": [
    {
      "org": "acme",
      "name": "code-review",
      "owner_email": "owner@example.com",
      "latest_version": "2",
      "current_version": "1",
      "description": "Use when reviewing pull requests",
      "visibility": "org",
      "platform_tags": ["codex"],
      "updated_at": "2026-05-06T17:42:11Z"
    }
  ]
}
```

`GET /v1/skills` and `GET /v1/orgs/{org}/skills` return the same row shape
under `skills`. Supported filters are additive on top of permissions:
`org`, `q`/`query`/`search`, repeatable `platform`,
`visibility=private|org|team`, `team=<slug>`, `owner=<email>`,
`sort=name|updated|owner|installs`, and `limit=N`. `limit` truncates the
returned array after permission and filter checks; V0 does not expose cursors,
offsets, or total counts.

Sort order is stable and permission-scoped. `name` sorts by org then skill
name ascending. `updated` sorts newest first, then org/name. `owner` sorts by
`owner_email`, then org/name. `installs` sorts by `install_count` descending,
then org/name; rows without install metrics sort last. Servers predating
install metrics reject `sort=installs` with `validation_error`. Owner and sort
filters do not expose hidden candidate or private rows; permission filtering
happens first.

`owner_email` is the V0 ownership contact. For skills it is derived from the
first publisher of the skill. For stacks it is derived from the stack creator.
It is visible to callers who can see the skill or stack and should be treated
as governance/contact metadata, not anonymous usage data. Multiple maintainers
and ownership transfer are deferred.

## Install Metrics

Skill metadata responses and catalog list/search rows may include two optional
install metrics:

- `install_count` — integer count of archive downloads the registry has
  served for the skill across all versions, through install and export.
- `last_installed_at` — RFC3339 timestamp of the most recent archive
  download. Omitted, or `null`, when no archive has been downloaded.

Both fields are optional for backward compatibility: servers MAY omit them,
and clients MUST tolerate their absence. Counts increment on the archive
download route
`GET /v1/orgs/{org}/skills/{name}/versions/{version}/archive`. Install
metrics are aggregate usage signals visible to callers who can see the skill;
V1 records no per-user attribution.

## Timestamps

Full skill metadata `updated_at` is the skill resource update time. Catalog
list/search row `updated_at` is permission-scoped, and `sort=updated` sorts by
that same returned value. Reader-only tokens see the current approved version's
`created_at` timestamp. Publisher/admin tokens see the skill catalog
`updated_at` timestamp, except when the latest version is hidden from the row,
where the value falls back to the visible version's `created_at` timestamp.
Stack `updated_at` is the stack resource update time. Treat `updated_at` as
activity metadata visible to callers who can see the resource. Registry
timestamps are registry-set UTC RFC3339 strings with a trailing `Z` and no
fractional seconds.

## Versions

```json
{
  "versions": [
    {
      "version": "2",
      "hash": { "algorithm": "sha256", "hex": "<64 hex chars>" },
      "platform_tags": ["codex"],
      "created_at": "2026-05-06T17:42:11Z",
      "status": "candidate",
      "current": false
    }
  ]
}
```

The registry should return visible versions newest-first.

## Skill Impact

`GET /v1/orgs/{org}/skills/{name}/impact` returns only stacks visible to the
caller. Empty usage is a successful response with `used_by: []`; callers must
not infer that hidden stacks do not exist.

```json
{
  "skill": {
    "org": "acme",
    "name": "code-review",
    "latest_version": "2",
    "current_version": "1",
    "description": "Use when reviewing pull requests",
    "visibility": "org"
  },
  "summary": {
    "used_by_count": 2,
    "current_policy_count": 1,
    "pinned_count": 1,
    "visible_only": true
  },
  "used_by": [
    {
      "stack": "acme/engineering-default",
      "org": "acme",
      "slug": "engineering-default",
      "name": "Engineering Default",
      "visibility": "org",
      "version_policy": "current",
      "effective_version": "1",
      "status": "approved",
      "current": true
    }
  ]
}
```

Rows may also include `owner_email`, `team`, `pinned_version`, `yanked_at`,
`yank_reason`, `deprecated_at`, and `deprecation_reason` when present.
`effective_version` and `status` are absent when a current-policy stack item has
no approved current version.

## Stacks

Stack create:

```json
{
  "slug": "engineering-default",
  "name": "Engineering Default",
  "description": "Default engineering skills",
  "visibility": "team",
  "team": "engineering"
}
```

Stack item:

```json
{ "skill": "code-review", "version_policy": "current" }
```

`GET /v1/orgs/{org}/stacks` accepts optional `team=<slug>`, `owner=<email>`,
and `limit=N` filters and returns:

```json
{
  "stacks": [
    {
      "org": "acme",
      "slug": "engineering-default",
      "name": "Engineering Default",
      "description": "Default engineering skills",
      "owner_email": "owner@example.com",
      "visibility": "team",
      "team": "engineering",
      "item_count": 3,
      "created_at": "2026-05-06T17:42:11Z",
      "updated_at": "2026-05-06T17:42:11Z"
    }
  ]
}
```

Stack list supports `owner=<email>` and `limit=N` after permission filtering.

Resolve response:

```json
{
  "stack": {
    "org": "acme",
    "slug": "engineering-default",
    "name": "Engineering Default",
    "visibility": "team",
    "team": "engineering"
  },
  "resolved_at": "2026-05-10T12:00:00Z",
  "manifest_hash": { "algorithm": "sha256", "hex": "<64 hex chars>" },
  "items": [
    {
      "skill": "code-review",
      "version_id": "ver_123",
      "version": "1",
      "archive_hash": { "algorithm": "sha256", "hex": "<64 hex chars>" },
      "download": {
        "method": "GET",
        "url": "/v1/orgs/acme/skills/code-review/versions/1/archive"
      },
      "version_policy": "current"
    }
  ]
}
```

Stack mutation responses include `audit_event_id`. The CLI may nest this id
inside the returned `stack` object for stack create/add/remove responses; the
registry envelope exposes it top-level.

## Visibility PATCH

Visibility PATCH requests accept visibility changes. `team` is required when
setting team visibility and invalid for `private` or `org`:

```json
{ "visibility": "team", "team": "engineering" }
```

Skill visibility PATCH returns updated skill metadata plus `audit_event_id`.
Stack visibility PATCH returns `{ "stack": ..., "audit_event_id": ... }`.
They do not approve, yank, deprecate, or otherwise move skill version state.

## Audit Events

Remote mutations create audit events. JSON responses for those mutations
include `audit_event_id`; callers can inspect it through
`/v1/orgs/{org}/audit/{event_id}`.

Audit events must not contain bearer tokens, token hashes, raw archive bytes,
or secret environment values.

## Error Envelope

```json
{
  "error": {
    "code": "skill_not_found",
    "message": "no such skill `acme/missing`",
    "http_status": 404
  }
}
```

Minimum stable codes:

| Code | HTTP | Meaning |
| --- | --- | --- |
| `bad_request` | 400 | Malformed request. |
| `validation_error` | 400 | Metadata or path parameter failed validation. |
| `hash_mismatch` | 400 | Archive bytes do not match declared hash. |
| `visibility_mismatch` | 400 | Resource visibility compatibility failed. |
| `unauthenticated` | 401 | Missing or invalid token. |
| `forbidden` | 403 | Token lacks permission. |
| `oauth_denied` | 401 | Provider login was denied or cancelled. |
| `oauth_expired` | 401 | OAuth state is invalid, expired, or already consumed. |
| `oauth_invalid_grant` | 401 | Provider code or PKCE verifier was invalid. |
| `invite_required` | 403 | Verified OAuth identity has no active invite or accepted identity. |
| `skill_not_found` | 404 | Skill missing or not visible. |
| `team_not_found` | 404 | Team missing or not visible. |
| `version_not_found` | 404 | Version missing or not visible. |
| `stack_not_found` | 404 | Stack missing or not visible. |
| `audit_event_not_found` | 404 | Audit event missing or not visible. |
| `no_current_version` | 409 | Candidate-only skill has no current approved version. |
| `stack_resolution_failed` | 409 | Stack cannot resolve to installable approved child versions. |
| `already_yanked` | 409 | Version is already yanked. |
| `already_deprecated` | 409 | Version is already deprecated. |
| `version_yanked` | 410 | Archive request refused for a yanked version. |
| `payload_too_large` | 413 | Archive exceeds configured limit. |
| `quota_exceeded` | 409 | Hosted safety limit was reached; request was rejected before creating a new bounded resource. |
| `audit_failed` | 500 | Governance mutation was rolled back because its audit row could not be written. |
| `internal_error` | 500 | Unexpected registry failure. |

Reserved future code: `rate_limited` / 429. V1 does not implement application
rate limiting; enforce limits at the edge or load balancer.

## Determinism

`agentstack skill pack` produces deterministic archives: sorted entries,
zeroed mtimes, dropped ownership, and stable gzip metadata. The registry stores
and serves bytes by verified SHA-256.
