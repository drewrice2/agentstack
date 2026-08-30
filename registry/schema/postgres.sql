-- Fresh PostgreSQL schema for a local AgentStack registry.
-- This is a fresh local schema, not a migration target.

CREATE TABLE users (
    id TEXT PRIMARY KEY,
    email TEXT NOT NULL UNIQUE,
    name TEXT,
    is_server_admin BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (email = lower(email)),
    CHECK (position('@' in email) > 1 AND position('@' in email) < length(email))
);

CREATE TABLE schema_migrations (
    id TEXT PRIMARY KEY,
    checksum_sha256 TEXT NOT NULL,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    applied_by_build TEXT,
    execution_ms BIGINT NOT NULL,
    CHECK (id ~ '^[0-9]{8}_[a-z0-9_]+$'),
    CHECK (checksum_sha256 ~ '^[0-9a-f]{64}$'),
    CHECK (execution_ms >= 0)
);

CREATE TABLE orgs (
    id TEXT PRIMARY KEY,
    slug TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (slug ~ '^[a-z0-9][a-z0-9_-]*$')
);

CREATE TABLE principals (
    id TEXT PRIMARY KEY,
    principal_type TEXT NOT NULL CHECK (principal_type IN ('human', 'machine')),
    display_name TEXT NOT NULL,
    is_server_admin BOOLEAN NOT NULL DEFAULT false,
    disabled_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (id ~ '^prn_[0-9a-f]{32}$'),
    CHECK (btrim(display_name) <> '')
);

CREATE TABLE human_profiles (
    principal_id TEXT PRIMARY KEY REFERENCES principals(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL UNIQUE REFERENCES users(id) ON DELETE CASCADE,
    email TEXT NOT NULL UNIQUE,
    name TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (email = lower(email)),
    CHECK (position('@' in email) > 1 AND position('@' in email) < length(email))
);

CREATE TABLE external_identities (
    id TEXT PRIMARY KEY,
    principal_id TEXT NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    issuer TEXT NOT NULL,
    subject TEXT NOT NULL,
    email TEXT NOT NULL,
    email_verified BOOLEAN NOT NULL,
    name TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at TIMESTAMPTZ,
    UNIQUE (issuer, subject),
    CHECK (id ~ '^ext_[0-9a-f]{32}$'),
    CHECK (provider ~ '^[a-z0-9][a-z0-9_-]*$'),
    CHECK (btrim(issuer) <> ''),
    CHECK (btrim(subject) <> ''),
    CHECK (email = lower(email)),
    CHECK (position('@' in email) > 1 AND position('@' in email) < length(email))
);

CREATE TABLE org_members (
    org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role TEXT NOT NULL CHECK (role IN ('org_admin', 'publisher', 'reader')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (org_id, user_id)
);

CREATE TABLE invites (
    id TEXT PRIMARY KEY,
    email TEXT NOT NULL,
    org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    role TEXT NOT NULL CHECK (role IN ('org_admin', 'publisher', 'reader')),
    invited_by_principal_id TEXT REFERENCES principals(id) ON DELETE SET NULL,
    accepted_by_principal_id TEXT REFERENCES principals(id) ON DELETE SET NULL,
    revoked_by_principal_id TEXT REFERENCES principals(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    accepted_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ,
    CHECK (id ~ '^inv_[0-9a-f]{32}$'),
    CHECK (email = lower(email)),
    CHECK (position('@' in email) > 1 AND position('@' in email) < length(email)),
    CHECK (expires_at > created_at),
    CHECK ((accepted_at IS NULL) = (accepted_by_principal_id IS NULL)),
    CHECK ((revoked_at IS NULL) = (revoked_by_principal_id IS NULL)),
    CHECK (accepted_at IS NULL OR revoked_at IS NULL)
);

CREATE TABLE tokens (
    id TEXT PRIMARY KEY,
    user_id TEXT REFERENCES users(id) ON DELETE CASCADE,
    principal_id TEXT NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
    label TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    token_kind TEXT NOT NULL DEFAULT 'user' CHECK (token_kind IN ('user', 'machine')),
    scopes JSONB NOT NULL DEFAULT '["registry:*"]'::jsonb,
    created_by_principal_id TEXT REFERENCES principals(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ,
    last_used_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ,
    CHECK (token_hash ~ '^[0-9a-f]{64}$'),
    CHECK (jsonb_typeof(scopes) = 'array'),
    CONSTRAINT tokens_user_kind_check CHECK ((token_kind = 'user') = (user_id IS NOT NULL))
);

CREATE TABLE ui_sessions (
    id TEXT PRIMARY KEY,
    token_id TEXT NOT NULL REFERENCES tokens(id) ON DELETE CASCADE,
    session_hash TEXT NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    last_used_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ,
    CHECK (session_hash ~ '^[0-9a-f]{64}$')
);

CREATE TABLE browser_sessions (
    id TEXT PRIMARY KEY,
    principal_id TEXT NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
    session_hash TEXT NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    last_used_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ,
    CHECK (id ~ '^brs_[0-9a-f]{32}$'),
    CHECK (session_hash ~ '^[0-9a-f]{64}$'),
    CHECK (expires_at > created_at)
);

CREATE TABLE oauth_login_states (
    id TEXT PRIMARY KEY,
    state_hash TEXT NOT NULL UNIQUE,
    nonce_hash TEXT NOT NULL,
    code_verifier_secret TEXT NOT NULL,
    redirect_after_path TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    consumed_at TIMESTAMPTZ,
    CHECK (id ~ '^ols_[0-9a-f]{32}$'),
    CHECK (state_hash ~ '^[0-9a-f]{64}$'),
    CHECK (nonce_hash ~ '^[0-9a-f]{64}$'),
    CHECK (btrim(code_verifier_secret) <> ''),
    CHECK (expires_at > created_at)
);

CREATE TABLE machine_principals (
    id TEXT PRIMARY KEY,
    principal_id TEXT NOT NULL UNIQUE REFERENCES principals(id) ON DELETE CASCADE,
    org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    slug TEXT NOT NULL,
    display_name TEXT NOT NULL,
    owner_principal_id TEXT REFERENCES principals(id) ON DELETE SET NULL,
    created_by_principal_id TEXT REFERENCES principals(id) ON DELETE SET NULL,
    disabled_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (org_id, slug),
    CHECK (id ~ '^mch_[0-9a-f]{32}$'),
    CHECK (slug ~ '^[a-z0-9][a-z0-9_-]*$'),
    CHECK (btrim(display_name) <> '')
);

CREATE TABLE teams (
    id TEXT PRIMARY KEY,
    org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    slug TEXT NOT NULL,
    name TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (org_id, slug),
    UNIQUE (id, org_id),
    CHECK (slug ~ '^[a-z0-9][a-z0-9_-]*$')
);

CREATE TABLE team_memberships (
    team_id TEXT NOT NULL,
    org_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('member', 'team_admin', 'lead')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (team_id, user_id),
    FOREIGN KEY (team_id, org_id) REFERENCES teams(id, org_id) ON DELETE CASCADE,
    FOREIGN KEY (org_id, user_id) REFERENCES org_members(org_id, user_id) ON DELETE CASCADE
);

CREATE TABLE archives (
    id TEXT PRIMARY KEY,
    hash_algorithm TEXT NOT NULL CHECK (hash_algorithm = 'sha256'),
    hash_hex TEXT NOT NULL,
    storage_key TEXT NOT NULL UNIQUE,
    size_bytes BIGINT NOT NULL CHECK (size_bytes > 0 AND size_bytes <= 52428800),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (hash_algorithm, hash_hex),
    CHECK (hash_hex ~ '^[0-9a-f]{64}$')
);

CREATE TABLE skills (
    id TEXT PRIMARY KEY,
    org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    visibility TEXT NOT NULL CHECK (visibility IN ('private', 'org', 'team')),
    team_id TEXT,
    owner_user_id TEXT NOT NULL REFERENCES users(id),
    current_version_id TEXT,
    next_version_number BIGINT NOT NULL DEFAULT 1 CHECK (next_version_number > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (org_id, name),
    UNIQUE (id, org_id),
    CHECK (name ~ '^[a-z0-9][a-z0-9_-]*$'),
    CHECK ((visibility = 'team') = (team_id IS NOT NULL)),
    FOREIGN KEY (org_id, owner_user_id) REFERENCES org_members(org_id, user_id),
    FOREIGN KEY (team_id, org_id) REFERENCES teams(id, org_id)
);

CREATE TABLE skill_versions (
    id TEXT PRIMARY KEY,
    skill_id TEXT NOT NULL REFERENCES skills(id) ON DELETE CASCADE,
    version_number BIGINT NOT NULL CHECK (version_number > 0),
    archive_id TEXT NOT NULL REFERENCES archives(id),
    description TEXT NOT NULL,
    published_by_user_id TEXT NOT NULL REFERENCES users(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    status TEXT NOT NULL DEFAULT 'candidate'
        CHECK (status IN ('candidate', 'approved', 'rejected')),
    approved_by_user_id TEXT REFERENCES users(id),
    approved_at TIMESTAMPTZ,
    yanked_by_user_id TEXT REFERENCES users(id),
    yanked_at TIMESTAMPTZ,
    yank_reason TEXT,
    deprecated_by_user_id TEXT REFERENCES users(id),
    deprecated_at TIMESTAMPTZ,
    deprecation_reason TEXT,
    UNIQUE (skill_id, version_number),
    UNIQUE (id, skill_id),
    CHECK ((status = 'approved') = (approved_at IS NOT NULL)),
    CHECK ((approved_at IS NULL) = (approved_by_user_id IS NULL)),
    CHECK (
        (yanked_at IS NULL AND yanked_by_user_id IS NULL AND yank_reason IS NULL)
        OR
        (yanked_at IS NOT NULL AND yanked_by_user_id IS NOT NULL AND yank_reason IS NOT NULL)
    ),
    CHECK (
        (deprecated_at IS NULL AND deprecated_by_user_id IS NULL AND deprecation_reason IS NULL)
        OR
        (deprecated_at IS NOT NULL AND deprecated_by_user_id IS NOT NULL AND deprecation_reason IS NOT NULL)
    )
);

ALTER TABLE skills
    ADD CONSTRAINT skills_current_version_fk
    FOREIGN KEY (current_version_id, id) REFERENCES skill_versions(id, skill_id);

CREATE TABLE skill_version_platform_tags (
    skill_version_id TEXT NOT NULL REFERENCES skill_versions(id) ON DELETE CASCADE,
    tag TEXT NOT NULL,
    PRIMARY KEY (skill_version_id, tag),
    CHECK (tag ~ '^[a-z0-9][a-z0-9._-]*$')
);

CREATE TABLE stacks (
    id TEXT PRIMARY KEY,
    org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    slug TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    visibility TEXT NOT NULL CHECK (visibility IN ('private', 'org', 'team')),
    team_id TEXT,
    owner_user_id TEXT NOT NULL REFERENCES users(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (org_id, slug),
    UNIQUE (id, org_id),
    CHECK (slug ~ '^[a-z0-9][a-z0-9_-]*$'),
    CHECK ((visibility = 'team') = (team_id IS NOT NULL)),
    FOREIGN KEY (org_id, owner_user_id) REFERENCES org_members(org_id, user_id),
    FOREIGN KEY (team_id, org_id) REFERENCES teams(id, org_id)
);

CREATE TABLE stack_items (
    id TEXT PRIMARY KEY,
    stack_id TEXT NOT NULL REFERENCES stacks(id) ON DELETE CASCADE,
    skill_id TEXT NOT NULL REFERENCES skills(id),
    version_policy TEXT NOT NULL CHECK (version_policy IN ('current', 'pinned')),
    pinned_version_id TEXT,
    position BIGINT NOT NULL CHECK (position > 0),
    added_by_user_id TEXT NOT NULL REFERENCES users(id),
    added_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (stack_id, skill_id),
    UNIQUE (stack_id, position),
    CHECK ((version_policy = 'pinned') = (pinned_version_id IS NOT NULL)),
    FOREIGN KEY (pinned_version_id, skill_id) REFERENCES skill_versions(id, skill_id)
);

CREATE TABLE audit_log (
    id TEXT PRIMARY KEY,
    org_id TEXT REFERENCES orgs(id) ON DELETE SET NULL,
    actor_user_id TEXT REFERENCES users(id) ON DELETE SET NULL,
    actor_principal_id TEXT REFERENCES principals(id) ON DELETE SET NULL,
    actor_type TEXT CHECK (actor_type IN ('human', 'machine')),
    action TEXT NOT NULL,
    resource_type TEXT NOT NULL CHECK (resource_type IN ('org', 'user', 'token', 'team', 'skill', 'stack')),
    resource_id TEXT,
    resource_ref TEXT,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (jsonb_typeof(metadata) = 'object')
);

CREATE INDEX idx_org_members_user_id ON org_members(user_id);
CREATE INDEX idx_principals_type ON principals(principal_type);
CREATE INDEX idx_human_profiles_user_id ON human_profiles(user_id);
CREATE INDEX idx_external_identities_principal_id ON external_identities(principal_id);
CREATE INDEX idx_invites_org_email ON invites(org_id, email) WHERE accepted_at IS NULL AND revoked_at IS NULL;
CREATE INDEX idx_tokens_user_id ON tokens(user_id);
CREATE INDEX idx_tokens_principal_id ON tokens(principal_id);
CREATE INDEX idx_tokens_active_user_id ON tokens(user_id) WHERE revoked_at IS NULL;
CREATE INDEX idx_ui_sessions_token_id ON ui_sessions(token_id);
CREATE INDEX idx_ui_sessions_active_hash ON ui_sessions(session_hash) WHERE revoked_at IS NULL;
CREATE INDEX idx_browser_sessions_principal_id ON browser_sessions(principal_id);
CREATE INDEX idx_browser_sessions_active_hash ON browser_sessions(session_hash) WHERE revoked_at IS NULL;
CREATE INDEX idx_oauth_login_states_expires ON oauth_login_states(expires_at);
CREATE INDEX idx_machine_principals_org_id ON machine_principals(org_id);
CREATE INDEX idx_machine_principals_principal_id ON machine_principals(principal_id);
CREATE INDEX idx_teams_org_id ON teams(org_id);
CREATE INDEX idx_team_memberships_user_id ON team_memberships(user_id);
CREATE INDEX idx_team_memberships_org_user ON team_memberships(org_id, user_id);

CREATE INDEX idx_skills_org_updated ON skills(org_id, updated_at DESC);
CREATE INDEX idx_skills_owner_user_id ON skills(owner_user_id);
CREATE INDEX idx_skills_team_id ON skills(team_id);
CREATE INDEX idx_skills_visibility ON skills(visibility);

CREATE INDEX idx_skill_versions_skill_version_desc ON skill_versions(skill_id, version_number DESC);
CREATE INDEX idx_skill_versions_archive_id ON skill_versions(archive_id);
CREATE INDEX idx_skill_versions_published_by_user_id ON skill_versions(published_by_user_id);
CREATE INDEX idx_skill_versions_status ON skill_versions(status);
CREATE INDEX idx_skill_version_platform_tags_tag ON skill_version_platform_tags(tag);

CREATE INDEX idx_stacks_org_updated ON stacks(org_id, updated_at DESC);
CREATE INDEX idx_stacks_owner_user_id ON stacks(owner_user_id);
CREATE INDEX idx_stacks_team_id ON stacks(team_id);
CREATE INDEX idx_stack_items_stack_id ON stack_items(stack_id, position);
CREATE INDEX idx_stack_items_skill_id ON stack_items(skill_id);

CREATE INDEX idx_audit_log_org_created ON audit_log(org_id, created_at DESC);
CREATE INDEX idx_audit_log_actor_created ON audit_log(actor_user_id, created_at DESC);
CREATE INDEX idx_audit_log_actor_principal_created ON audit_log(actor_principal_id, created_at DESC);
CREATE INDEX idx_audit_log_resource ON audit_log(resource_type, resource_id, created_at DESC);

CREATE OR REPLACE FUNCTION set_updated_at()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    NEW.updated_at = now();
    RETURN NEW;
END;
$$;

CREATE TRIGGER users_set_updated_at
BEFORE UPDATE ON users
FOR EACH ROW
EXECUTE FUNCTION set_updated_at();

CREATE TRIGGER orgs_set_updated_at
BEFORE UPDATE ON orgs
FOR EACH ROW
EXECUTE FUNCTION set_updated_at();

CREATE TRIGGER principals_set_updated_at
BEFORE UPDATE ON principals
FOR EACH ROW
EXECUTE FUNCTION set_updated_at();

CREATE TRIGGER human_profiles_set_updated_at
BEFORE UPDATE ON human_profiles
FOR EACH ROW
EXECUTE FUNCTION set_updated_at();

CREATE TRIGGER org_members_set_updated_at
BEFORE UPDATE ON org_members
FOR EACH ROW
EXECUTE FUNCTION set_updated_at();

CREATE TRIGGER machine_principals_set_updated_at
BEFORE UPDATE ON machine_principals
FOR EACH ROW
EXECUTE FUNCTION set_updated_at();

CREATE TRIGGER teams_set_updated_at
BEFORE UPDATE ON teams
FOR EACH ROW
EXECUTE FUNCTION set_updated_at();

CREATE TRIGGER team_memberships_set_updated_at
BEFORE UPDATE ON team_memberships
FOR EACH ROW
EXECUTE FUNCTION set_updated_at();

CREATE TRIGGER skills_set_updated_at
BEFORE UPDATE ON skills
FOR EACH ROW
EXECUTE FUNCTION set_updated_at();

CREATE TRIGGER stacks_set_updated_at
BEFORE UPDATE ON stacks
FOR EACH ROW
EXECUTE FUNCTION set_updated_at();

CREATE OR REPLACE FUNCTION require_current_version_approved()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.current_version_id IS NOT NULL AND NOT EXISTS (
        SELECT 1
        FROM skill_versions
        WHERE skill_versions.id = NEW.current_version_id
          AND skill_versions.skill_id = NEW.id
          AND skill_versions.status = 'approved'
    ) THEN
        RAISE EXCEPTION 'current_version_id must reference an approved version for the same skill';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER skills_current_version_check
BEFORE INSERT OR UPDATE OF current_version_id ON skills
FOR EACH ROW
EXECUTE FUNCTION require_current_version_approved();

CREATE OR REPLACE FUNCTION prevent_current_version_unapprove()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF OLD.status = 'approved'
       AND NEW.status <> 'approved'
       AND EXISTS (
           SELECT 1
           FROM skills
           WHERE skills.current_version_id = OLD.id
       ) THEN
        RAISE EXCEPTION 'current version must remain approved';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER skill_versions_current_status_update_check
BEFORE UPDATE OF status ON skill_versions
FOR EACH ROW
EXECUTE FUNCTION prevent_current_version_unapprove();

CREATE OR REPLACE FUNCTION advance_skill_next_version_number()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    UPDATE skills
    SET next_version_number = greatest(next_version_number, NEW.version_number + 1)
    WHERE id = NEW.skill_id;
    RETURN NEW;
END;
$$;

CREATE TRIGGER skill_versions_advance_next_version_number
AFTER INSERT ON skill_versions
FOR EACH ROW
EXECUTE FUNCTION advance_skill_next_version_number();

CREATE OR REPLACE FUNCTION require_skill_version_publisher_member()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM skills
        JOIN org_members ON org_members.org_id = skills.org_id
                        AND org_members.user_id = NEW.published_by_user_id
        WHERE skills.id = NEW.skill_id
    ) THEN
        RAISE EXCEPTION 'published_by_user_id must be a member of the skill org';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER skill_versions_publisher_member_check
BEFORE INSERT OR UPDATE OF skill_id, published_by_user_id ON skill_versions
FOR EACH ROW
EXECUTE FUNCTION require_skill_version_publisher_member();

CREATE OR REPLACE FUNCTION require_stack_item_adder_member()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM stacks
        JOIN org_members ON org_members.org_id = stacks.org_id
                        AND org_members.user_id = NEW.added_by_user_id
        WHERE stacks.id = NEW.stack_id
    ) THEN
        RAISE EXCEPTION 'added_by_user_id must be a member of the stack org';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER stack_items_adder_member_check
BEFORE INSERT OR UPDATE OF stack_id, added_by_user_id ON stack_items
FOR EACH ROW
EXECUTE FUNCTION require_stack_item_adder_member();
