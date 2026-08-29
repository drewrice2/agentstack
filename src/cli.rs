use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;

use crate::skill::DEFAULT_SOFT_CHAR_LIMIT;

/// CLI parser for the `agentstack` binary.
#[derive(Debug, Parser)]
#[command(
    name = "agentstack",
    version,
    about = "Package, install, and share portable AI-agent skills",
    long_about = "AgentStack packages, installs, and updates portable AI-agent \
                  skills and stacks.\n\n\
                  Local authoring needs no registry.\n\
                  Sharing, approval, and stacks use a private registry.\n\
                  It does not execute agents.",
    propagate_version = true,
    arg_required_else_help = true,
    after_help = "Start:\n  \
                  agentstack doctor\n  \
                  agentstack skill init my-skill --name my-skill --description \"Use when reviewing PRs\"\n  \
                  agentstack skill validate ./my-skill\n  \
                  agentstack skill install ./my-skill --target local\n\n\
                  Publish:\n  \
                  agentstack auth login\n  \
                  agentstack registry ping --auth\n  \
                  agentstack skill push ./my-skill --org acme --scope org\n  \
                  agentstack skill version approve acme/my-skill@1\n  \
                  agentstack stack install acme/engineering-default --target codex-repo\n\n\
                  Headless:\n  \
                  env AGENTSTACK_REGISTRY_URL=https://registry.agentstack.gg \\\n  \
                    AGENTSTACK_TOKEN_PATH=/run/secrets/agentstack_token \\\n  \
                    agentstack stack export acme/engineering-default --out ./skills --json --no-input\n\n\
                  Note: `acme` is an example org; replace it with your org.\n\
                  Refs: use `org/name` in scripts; bare names only when your token has one org.\n\
                  More: agentstack <command> --help; README.md."
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,

    #[command(subcommand)]
    pub command: Command,
}

/// Process-wide flags accepted before or after any subcommand.
#[derive(Debug, Args, Clone, Default)]
pub struct GlobalArgs {
    /// Emit structured JSON on stdout where the command supports it.
    #[arg(long, global = true, help_heading = "Global")]
    pub json: bool,

    /// Never prompt for input; fail or print next commands instead.
    #[arg(long, global = true, help_heading = "Global")]
    pub no_input: bool,

    /// Print diagnostic detail to stderr.
    #[arg(long, short = 'v', global = true, help_heading = "Global")]
    pub verbose: bool,

    /// Suppress non-essential human output. Errors still go to stderr.
    #[arg(
        long,
        short = 'q',
        global = true,
        conflicts_with = "verbose",
        help_heading = "Global"
    )]
    pub quiet: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Author, publish, inspect, export, and install skills.
    Skill {
        #[command(subcommand)]
        action: SkillCommand,
    },

    /// Create, inspect, resolve, export, and install stacks.
    Stack {
        #[command(subcommand)]
        action: StackCommand,
    },

    /// List receipts, explain installs, batch-update receipts, and diagnose targets.
    Install {
        #[command(subcommand)]
        action: InstallCommand,
    },

    /// Log in, log out, and inspect registry identity.
    Auth {
        #[command(subcommand)]
        action: AuthCommand,
    },

    /// Manage install targets and resolved target paths.
    Target {
        #[command(subcommand)]
        action: TargetCommand,
    },

    /// Inspect registry audit trails.
    Audit {
        #[command(subcommand)]
        action: AuditCommand,
    },

    /// Manage teams and team membership.
    Team {
        #[command(subcommand)]
        action: TeamsCommand,
    },

    /// Generate a shell-completion script on stdout.
    #[command(after_help = "Examples:\n  \
              agentstack completion zsh > ~/.zfunc/_agentstack\n  \
              agentstack completion bash > ~/.local/share/bash-completion/completions/agentstack\n  \
              agentstack completion fish > ~/.config/fish/completions/agentstack.fish")]
    Completion {
        /// Target shell.
        #[arg(value_enum, value_name = "SHELL")]
        shell: ShellArg,
    },

    /// Inspect or update local AgentStack configuration.
    #[command(after_help = "Examples:\n  \
              agentstack config path\n  \
              agentstack config show\n  \
              agentstack target set claude-code --path ~/.claude/skills")]
    Config {
        #[command(subcommand)]
        action: ConfigCommand,
    },

    /// Inspect or clean the local package cache.
    #[command(after_help = "Examples:\n  \
              agentstack cache path\n  \
              agentstack cache list\n  \
              agentstack cache remove code-review --force")]
    Cache {
        #[command(subcommand)]
        action: CacheCommand,
    },

    /// Manage the persisted registry URL and reachability check.
    #[command(after_help = "Examples:\n  \
              agentstack registry use https://registry.agentstack.gg\n  \
              agentstack registry show\n  \
              agentstack registry ping --auth")]
    Registry {
        #[command(subcommand)]
        action: RegistryCommand,
    },

    /// Converge install targets to a declarative skill/stack manifest.
    #[command(
        after_help = "Reads a TOML manifest and converges each declared install target:\n\
              installs missing skills and stacks, and updates installs that are\n\
              outdated or locally modified. Pinned skill refs stay on their pin.\n\
              `--prune` also removes receipt-backed installs in manifest targets\n\
              that the manifest no longer declares; unmanaged files are never\n\
              touched.\n\n\
              Manifest format (agentstack.toml):\n\n  \
              [[stacks]]\n  \
              ref = \"acme/engineering-default\"\n  \
              target = \"claude-code-repo\"\n\n  \
              [[skills]]\n  \
              ref = \"acme/code-review\"        # pin with @VERSION, e.g. \"acme/code-review@2\"\n  \
              target = \"codex-repo\"\n\n\
              Examples:\n  \
              agentstack sync --check\n  \
              agentstack sync --yes\n  \
              agentstack sync --manifest ./agentstack.toml --prune --yes"
    )]
    Sync {
        /// Manifest path.
        #[arg(long, value_name = "PATH", default_value = "agentstack.toml")]
        manifest: PathBuf,
        /// Report what would change without writing anything.
        #[arg(long)]
        check: bool,
        /// Remove undeclared receipt-backed installs from manifest targets.
        #[arg(long)]
        prune: bool,
        /// Skip the interactive confirmation prompt.
        #[arg(long)]
        yes: bool,
    },

    /// Check local config, cache, auth, registry, and install targets.
    #[command(after_help = "Examples:\n  \
              agentstack doctor\n  \
              agentstack doctor --json")]
    Doctor,
}

#[derive(Debug, Subcommand)]
pub enum SkillCommand {
    /// Create a skill directory skeleton.
    #[command(after_help = "Examples:\n  \
              agentstack skill init my-skill --name my-skill --description \"Use when reviewing PRs\"\n  \
              agentstack skill init ./skills/my-skill --name my-skill --description \"Use when reviewing PRs\"")]
    Init {
        /// Destination directory for the new skill (defaults to ./<name>).
        path: Option<PathBuf>,
        /// Skill name (lowercase letters, digits, hyphens; must start with a letter).
        #[arg(long, value_name = "NAME")]
        name: String,
        /// Trigger-oriented description shown in list/search output.
        #[arg(long, value_name = "DESCRIPTION")]
        description: String,
    },
    /// Check required SKILL.md structure.
    #[command(after_help = "Examples:\n  \
              agentstack skill validate ./my-skill\n  \
              agentstack skill validate ./my-skill --json")]
    Validate {
        /// Path to the skill directory (defaults to the current directory).
        path: Option<PathBuf>,
    },
    /// Run advisory skill-quality checks.
    #[command(after_help = "Examples:\n  \
              agentstack skill lint ./my-skill\n  \
              agentstack skill lint ./my-skill --json")]
    Lint {
        /// Path to the skill directory (defaults to the current directory).
        path: Option<PathBuf>,
        /// Soft character limit for SKILL.md before lint warns.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_SOFT_CHAR_LIMIT)]
        max_chars: usize,
    },
    /// Summarize metadata, structure, validation, and lint findings.
    #[command(after_help = "Examples:\n  \
              agentstack skill inspect ./my-skill\n  \
              agentstack skill inspect ./my-skill --json")]
    Inspect {
        /// Path to the skill directory (defaults to the current directory).
        path: Option<PathBuf>,
        /// Soft character limit applied to embedded lint checks.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_SOFT_CHAR_LIMIT)]
        max_chars: usize,
    },
    /// Scan packaged skill text for narrow security-risk phrases.
    #[command(after_help = "Examples:\n  \
              agentstack skill security-scan ./my-skill\n  \
              agentstack skill security-scan ./my-skill --json")]
    SecurityScan {
        /// Path to the skill directory (defaults to the current directory).
        path: Option<PathBuf>,
    },
    /// Scan a directory for local skill directories.
    #[command(after_help = "Examples:\n  \
              agentstack skill scan .\n  \
              agentstack skill scan ./skills --json")]
    Scan {
        /// Directory to scan for skills (defaults to the current directory).
        path: Option<PathBuf>,
    },
    /// Package a local skill archive.
    #[command(after_help = "Examples:\n  \
              agentstack skill pack ./my-skill\n  \
              agentstack skill pack ./my-skill --out my-skill.tar.gz --force")]
    Pack {
        /// Path to the skill directory (defaults to the current directory).
        path: Option<PathBuf>,
        /// Archive path to write (defaults to `<skill-name>.tar.gz`).
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
        /// Overwrite an existing archive path.
        #[arg(long)]
        force: bool,
        /// Skip writing a copy into the local cache.
        #[arg(long)]
        no_cache: bool,
    },
    /// Extract a packaged skill archive.
    Unpack {
        /// Path to a `.tar.gz` archive produced by `agentstack skill pack`.
        archive: PathBuf,
        /// Parent directory under which `<skill-name>/` is created.
        #[arg(long, value_name = "DIR")]
        out: PathBuf,
        /// Replace an existing `<out>/<skill-name>` directory.
        #[arg(long)]
        force: bool,
    },
    /// List registry skills visible to the active token.
    #[command(
        after_help = "Examples:\n  agentstack skill list\n  agentstack skill list --org acme --team platform\n  agentstack skill list --org acme --scope org --platform codex --limit 25\n  agentstack skill list --org acme --json"
    )]
    List {
        /// Restrict results to one org. Omit when the active token has one org.
        #[arg(long, value_name = "ORG")]
        org: Option<String>,
        /// Restrict results to one team-visible catalog.
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
        /// Restrict results to skills tagged for a platform. Repeat for OR matching.
        #[arg(long = "platform", value_name = "TAG")]
        platforms: Vec<String>,
        /// Restrict results to one visibility tier.
        #[arg(long, value_enum, value_name = "SCOPE")]
        scope: Option<ScopeArg>,
        /// Restrict results to one owner contact.
        #[arg(long, value_name = "EMAIL")]
        owner: Option<String>,
        /// Sort result rows.
        #[arg(long, value_enum, value_name = "FIELD")]
        sort: Option<CatalogSortArg>,
        /// Limit the number of returned rows.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
    },
    /// Search registry skills visible to the active token.
    #[command(
        after_help = "Examples:\n  agentstack skill search review\n  agentstack skill search review --org acme --team platform\n  agentstack skill search lint --org acme --platform codex --limit 25\n  agentstack skill search review --json"
    )]
    Search {
        /// Query matched against visible names and descriptions.
        query: String,
        /// Restrict results to one org. Omit when the active token has one org.
        #[arg(long, value_name = "ORG")]
        org: Option<String>,
        /// Restrict results to one team-visible catalog.
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
        /// Restrict results to skills tagged for a platform. Repeat for OR matching.
        #[arg(long = "platform", value_name = "TAG")]
        platforms: Vec<String>,
        /// Restrict results to one visibility tier.
        #[arg(long, value_enum, value_name = "SCOPE")]
        scope: Option<ScopeArg>,
        /// Restrict results to one owner contact.
        #[arg(long, value_name = "EMAIL")]
        owner: Option<String>,
        /// Sort result rows.
        #[arg(long, value_enum, value_name = "FIELD")]
        sort: Option<CatalogSortArg>,
        /// Limit the number of returned rows.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
    },
    /// List candidate versions awaiting approval across visible skills.
    #[command(
        after_help = "Aggregates client-side: lists visible skills, then fetches each skill's versions and keeps non-yanked candidates.\n\nExamples:\n  agentstack skill candidates\n  agentstack skill candidates --org acme\n  agentstack skill candidates --org acme --limit 25 --json"
    )]
    Candidates {
        /// Restrict the scan to one org. Omit when the active token has one org.
        #[arg(long, value_name = "ORG")]
        org: Option<String>,
        /// Limit the number of skills scanned for candidates.
        #[arg(long, value_name = "N", default_value_t = 100)]
        limit: usize,
    },
    /// Show registry metadata, or an installed copy when --target is set.
    #[command(after_help = "Examples:\n  \
              agentstack skill show acme/code-review\n  \
              agentstack skill show acme/code-review@2\n  \
              agentstack skill show code-review --target codex-repo\n  \
              agentstack skill show code-review --target local --json")]
    Show {
        /// Registry skill ref, or installed skill name when --target is set.
        skill_ref: String,
        /// Require the skill ref to be visible to this team.
        #[arg(long, value_name = "TEAM", conflicts_with = "target")]
        team: Option<String>,
        /// Inspect the installed copy in this target instead of registry metadata.
        #[arg(long, value_name = "TARGET")]
        target: Option<String>,
    },
    /// Show registry lifecycle status for a skill.
    #[command(after_help = "Examples:\n  \
              agentstack skill status acme/code-review\n  \
              agentstack skill status code-review --team platform\n  \
              agentstack skill status acme/code-review --json")]
    Status {
        /// Skill ref `skill` or `org/skill`.
        skill_ref: String,
        /// Require the skill ref to be visible to this team.
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
    },
    /// Show registry stack impact for a skill.
    #[command(after_help = "Examples:\n  \
              agentstack skill impact acme/code-review\n  \
              agentstack skill impact code-review --team platform\n  \
              agentstack skill impact acme/code-review --json")]
    Impact {
        skill_ref: String,
        /// Require the skill ref to be visible to this team.
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
    },
    /// Compare package contents for local paths, registry refs, or an installed copy.
    #[command(after_help = "Examples:\n  \
              agentstack skill diff ./my-skill ./my-skill-v2\n  \
              agentstack skill diff ./my-skill code-review@2 --json\n  \
              agentstack skill diff acme/code-review@1 acme/code-review@2\n  \
              agentstack skill diff code-review --target codex-repo\n  \
              agentstack skill diff acme/code-review@3 --target claude-code-repo")]
    Diff {
        /// Left local skill path or registry ref `skill[@version]` or `org/skill[@version]`.
        /// With --target, the installed skill to compare; an inline `@version` pins the
        /// registry side instead of the current approved version.
        left: String,
        /// Right local skill path or registry ref `skill[@version]` or `org/skill[@version]`.
        /// Omit when --target is set.
        #[arg(required_unless_present = "target", conflicts_with = "target")]
        right: Option<String>,
        /// Compare the installed copy in this target against the registry.
        #[arg(long, value_name = "TARGET")]
        target: Option<String>,
        /// Server-admin recovery only: permit comparing pinned yanked registry versions.
        #[arg(long)]
        allow_yanked: bool,
    },
    /// Upload a candidate skill version.
    #[command(
        disable_version_flag = true,
        after_help = "Examples:\n  \
              agentstack skill push ./my-skill\n  \
              agentstack skill push ./my-skill --org acme\n  \
              agentstack skill push ./my-skill --org acme --scope org\n  \
              agentstack skill push ./my-skill --org acme --scope team --team platform\n  \
              agentstack skill push --all ./skills --include 'code-*' --yes\n  \
              agentstack skill push ./my-skill --dry-run"
    )]
    Push {
        /// Path to the skill directory, or its parent when using `--all`
        /// (defaults to the current directory).
        path: Option<PathBuf>,
        /// Push every skill found directly under `path`.
        #[arg(long)]
        all: bool,
        /// Organization slug to publish under. Omit when the active token has one org.
        #[arg(long, value_name = "ORG")]
        org: Option<String>,
        /// Visibility tier for the uploaded version.
        #[arg(long, value_enum, value_name = "SCOPE", default_value_t = ScopeArg::Private)]
        scope: ScopeArg,
        /// Team that may see the version when `--scope team` is used.
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
        /// Tag the version for a platform. Repeat to tag several.
        #[arg(long = "platform", value_name = "TAG")]
        platforms: Vec<String>,
        /// With `--all`, only push skills whose directory matches this glob. Repeatable.
        #[arg(long, value_name = "GLOB", requires = "all")]
        include: Vec<String>,
        /// With `--all`, skip skills whose directory matches this glob. Repeatable.
        #[arg(long, value_name = "GLOB", requires = "all")]
        exclude: Vec<String>,
        /// Skip the interactive confirmation prompt.
        #[arg(long)]
        yes: bool,
        /// Show what would be uploaded without contacting the registry.
        #[arg(long)]
        dry_run: bool,
    },
    /// Bulk-import local skills into the registry as candidate versions.
    #[command(after_help = "Examples:\n  \
              agentstack skill adopt\n  \
              agentstack skill adopt ./skills --org acme\n  \
              agentstack skill adopt ./skills --org acme --scope org --yes\n  \
              agentstack skill adopt ./skills --org acme --dry-run --json")]
    Adopt {
        /// Directory to scan for skills (defaults to the current directory).
        path: Option<PathBuf>,
        /// Organization slug to publish under. Omit when the active token has one org.
        #[arg(long, value_name = "ORG")]
        org: Option<String>,
        /// Visibility tier for the uploaded versions.
        #[arg(long, value_enum, value_name = "SCOPE", default_value_t = ScopeArg::Private)]
        scope: ScopeArg,
        /// Team that may see the versions when `--scope team` is used.
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
        /// Tag the versions for a platform. Repeat to tag several.
        #[arg(long = "platform", value_name = "TAG")]
        platforms: Vec<String>,
        /// Show what would be uploaded without contacting the registry.
        #[arg(long)]
        dry_run: bool,
        /// Skip the interactive confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Export an unmanaged skill directory from the registry.
    #[command(
        disable_version_flag = true,
        after_help = "Examples:\n  \
                      agentstack skill export acme/code-review --out ./skills\n  \
                      agentstack skill export code-review --out ./skills\n  \
                      agentstack skill export acme/code-review@1 --out ./skills --dry-run --json"
    )]
    Export {
        /// Registry ref `skill`, `skill@version`, `org/skill`, or
        /// `org/skill@version`. Export is unmanaged and does not write an
        /// install receipt.
        skill_ref: String,
        /// Require the skill ref to be visible to this team.
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
        /// Output parent directory.
        #[arg(long, value_name = "DIR")]
        out: PathBuf,
        /// Replace an existing exported skill directory.
        #[arg(long)]
        force: bool,
        /// Resolve and verify without writing files.
        #[arg(long)]
        dry_run: bool,
        /// Server-admin recovery only: permit exporting a pinned yanked version.
        #[arg(long)]
        allow_yanked: bool,
    },
    /// Install a local or registry skill into a target runtime.
    #[command(after_help = "Examples:\n  \
              agentstack skill install ./my-skill --target local\n  \
              agentstack skill install code-review --target codex-repo\n  \
              agentstack skill install code-review --team platform --target codex-repo\n  \
              agentstack skill install acme/code-review --target codex-repo\n  \
              agentstack skill install acme/code-review@1 --target claude-code-repo")]
    Install {
        /// Local skill directory or registry ref `skill[@version]` or
        /// `org/skill[@version]`. A pinned registry ref installs that
        /// version, but `skill update` later follows the skill's current
        /// approved version.
        source: String,
        /// Require the registry ref to be visible to this team.
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
        /// Install target: codex, claude-code, codex-repo, claude-code-repo, or local.
        #[arg(long, value_name = "TARGET")]
        target: String,
        /// Replace an existing install at the target path.
        #[arg(long)]
        force: bool,
        /// Server-admin recovery only: permit installing a pinned yanked version.
        #[arg(long)]
        allow_yanked: bool,
    },
    /// Update an installed registry skill in a target runtime.
    #[command(after_help = "Examples:\n  \
              agentstack skill update code-review --target codex-repo --check\n  \
              agentstack skill update code-review --target local\n  \
              agentstack skill update code-review --target local --force")]
    Update {
        /// Installed skill name.
        skill: String,
        /// Install target: codex, claude-code, codex-repo, claude-code-repo, or local.
        #[arg(long, value_name = "TARGET")]
        target: String,
        /// Report what would change without writing anything.
        #[arg(long)]
        check: bool,
        /// Re-apply the current version even when the receipt looks up to date.
        #[arg(long)]
        force: bool,
    },
    /// Uninstall a skill from a target.
    #[command(after_help = "Examples:\n  \
              agentstack skill uninstall code-review --target codex-repo --dry-run\n  \
              agentstack skill uninstall code-review --target codex-repo --yes")]
    Uninstall {
        /// Installed skill name.
        skill: String,
        /// Install target: codex, claude-code, codex-repo, claude-code-repo, or local.
        #[arg(long, value_name = "TARGET")]
        target: String,
        /// Remove the skill directory even if receipt safety checks fail.
        #[arg(long)]
        force: bool,
        /// Skip the interactive confirmation prompt.
        #[arg(long)]
        yes: bool,
        /// Show what would be removed without deleting files.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show or change skill visibility.
    Visibility {
        #[command(subcommand)]
        action: SkillVisibilityCommand,
    },
    /// List audit events for a skill.
    #[command(after_help = "Examples:\n  \
              agentstack skill audit acme/code-review\n  \
              agentstack skill audit code-review --team platform\n  \
              agentstack skill audit acme/code-review --json")]
    Audit {
        /// Skill ref `skill` or `org/skill`.
        skill_ref: String,
        /// Require the skill ref to be visible to this team.
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
    },
    /// Manage uploaded skill versions.
    Version {
        #[command(subcommand)]
        action: SkillVersionCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum SkillVisibilityCommand {
    /// Show the current visibility scope.
    Show {
        /// Skill ref `skill` or `org/skill`.
        skill_ref: String,
        /// Require the skill ref to be visible to this team.
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
    },
    /// Change visibility scope without approving or moving versions.
    Set {
        /// Skill ref `skill` or `org/skill`.
        skill_ref: String,
        /// New visibility scope. Use `--team` only with `--scope team`.
        #[arg(long, value_enum, value_name = "SCOPE")]
        scope: ScopeArg,
        /// Team slug required when `--scope team` is used.
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum SkillVersionCommand {
    /// List uploaded versions.
    List {
        /// Skill ref `skill` or `org/skill`.
        skill_ref: String,
        /// Require the skill ref to be visible to this team.
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
    },
    /// Show one uploaded version.
    Show {
        /// Pinned skill ref `skill@version` or `org/skill@version`.
        skill_ref: String,
        /// Require the skill ref to be visible to this team.
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
    },
    /// Approve one uploaded version and make it current.
    #[command(
        disable_version_flag = true,
        after_help = "Approving moves the current version used by unpinned installs. Allowed for org_admin/server_admin, and team_admin for team-scoped skills.\nAudit with: agentstack skill audit <skill>\n\nExamples:\n  \
              agentstack skill version approve acme/code-review@2\n  \
              agentstack skill version approve code-review@2 --team platform\n  \
              agentstack skill version approve acme/code-review@2 --json"
    )]
    Approve {
        /// Pinned skill ref `skill@version` or `org/skill@version`.
        skill_ref: String,
        /// Require the skill ref to be visible to this team.
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
    },
    /// Mark a pinned version as yanked.
    Yank {
        /// Pinned skill ref `skill@version` or `org/skill@version`.
        skill_ref: String,
        /// Require the skill ref to be visible to this team.
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
        /// Short operator-facing reason recorded in audit/status output.
        #[arg(long, value_name = "REASON")]
        reason: String,
    },
    /// Mark a pinned version as deprecated.
    Deprecate {
        /// Pinned skill ref `skill@version` or `org/skill@version`.
        skill_ref: String,
        /// Require the skill ref to be visible to this team.
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
        /// Short operator-facing reason recorded in audit/status output.
        #[arg(long, value_name = "REASON")]
        reason: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum StackCommand {
    /// Create a registry stack.
    #[command(
        after_help = "Examples:\n  agentstack stack create engineering-default --scope org\n  agentstack stack create acme/engineering-default --scope org"
    )]
    Create {
        /// Stack ref `stack` or `org/stack`.
        stack_ref: String,
        #[arg(long, value_enum, value_name = "SCOPE", default_value_t = ScopeArg::Private)]
        scope: ScopeArg,
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        #[arg(long, value_name = "TEXT", default_value = "")]
        description: String,
    },
    /// List visible stacks for the active org.
    #[command(
        after_help = "Examples:\n  agentstack stack list\n  agentstack stack list --org acme\n  agentstack stack list --org acme --team platform\n  agentstack stack list --org acme --owner owner@example.com --limit 25\n  agentstack stack list --org acme --json"
    )]
    List {
        /// Restrict results to one org. Omit when the active token has one org.
        #[arg(long, value_name = "ORG")]
        org: Option<String>,
        /// Restrict results to one team-visible catalog.
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
        /// Restrict results to one owner contact.
        #[arg(long, value_name = "EMAIL")]
        owner: Option<String>,
        /// Limit the number of returned rows.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
    },
    /// Show stack metadata, or an installed copy when --target is set.
    #[command(after_help = "Examples:\n  \
              agentstack stack show acme/engineering-default\n  \
              agentstack stack show engineering-default --team platform\n  \
              agentstack stack show acme/engineering-default --target codex-repo\n  \
              agentstack stack show acme/engineering-default --target local --json")]
    Show {
        /// Stack ref `stack` or `org/stack`.
        stack_ref: String,
        /// Require the stack ref to be visible to this team.
        #[arg(long, value_name = "TEAM", conflicts_with = "target")]
        team: Option<String>,
        /// Inspect the installed copy in this target instead of registry metadata.
        #[arg(long, value_name = "TARGET")]
        target: Option<String>,
    },
    /// Show stack lifecycle status.
    #[command(after_help = "Examples:\n  \
              agentstack stack status acme/engineering-default\n  \
              agentstack stack status engineering-default --team platform\n  \
              agentstack stack status acme/engineering-default --json")]
    Status {
        /// Stack ref `stack` or `org/stack`.
        stack_ref: String,
        /// Require the stack ref to be visible to this team.
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
    },
    /// Add or update a skill in a stack.
    #[command(
        after_help = "Examples:\n  agentstack stack add engineering-default code-review\n  agentstack stack add engineering-default code-review@1 --version-policy pinned\n  agentstack stack add acme/engineering-default acme/code-review --version-policy current\n\nVersion policy defaults to `current`. Use `pinned` with either an inline `@version` ref or --pin-version."
    )]
    Add {
        /// Stack ref `stack` or `org/stack`.
        stack_ref: String,
        /// Require the stack ref to be visible to this team.
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
        /// Skill ref `skill[@version]` or `org/skill[@version]` from the same org.
        skill_ref: String,
        /// Version policy: `current` (follow approved current) or `pinned`.
        #[arg(long, value_name = "POLICY")]
        version_policy: Option<String>,
        /// Pin this stack item to a specific uploaded skill version.
        #[arg(long, value_name = "VERSION")]
        pin_version: Option<String>,
    },
    /// Remove a skill from a registry stack definition.
    #[command(after_help = "Examples:\n  \
              agentstack stack remove engineering-default code-review\n  \
              agentstack stack remove acme/engineering-default acme/code-review --dry-run\n  \
              agentstack stack remove acme/engineering-default acme/code-review --yes")]
    Remove {
        /// Stack ref `stack` or `org/stack`.
        stack_ref: String,
        /// Require the stack ref to be visible to this team.
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
        /// Skill ref `skill` or `org/skill` from the same org.
        skill_ref: String,
        /// Skip the interactive confirmation prompt.
        #[arg(long)]
        yes: bool,
        /// Show what would be removed without changing the stack.
        #[arg(long)]
        dry_run: bool,
    },
    /// Resolve a stack to concrete skill versions.
    Resolve {
        /// Stack ref `stack` or `org/stack`.
        stack_ref: String,
        /// Require the stack ref to be visible to this team.
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
    },
    /// Export unmanaged stack skills to an output directory.
    #[command(
        after_help = "Examples:\n  agentstack stack export engineering-default --out ./skills\n  agentstack stack export acme/engineering-default --out ./skills"
    )]
    Export {
        /// Stack ref `stack` or `org/stack`.
        stack_ref: String,
        /// Require the stack ref to be visible to this team.
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
        /// Output parent directory.
        #[arg(long, value_name = "DIR")]
        out: PathBuf,
        /// Replace existing exported child skill directories.
        #[arg(long)]
        force: bool,
        /// Resolve and verify without writing files.
        #[arg(long)]
        dry_run: bool,
    },
    /// Install a stack into a target runtime.
    #[command(
        after_help = "Examples:\n  agentstack stack install engineering-default --target codex-repo\n  agentstack stack install engineering-default --target claude-code-repo\n  agentstack stack install engineering-default --team platform --target codex-repo\n  agentstack stack install acme/engineering-default --target codex-repo"
    )]
    Install {
        /// Stack ref `stack` or `org/stack`.
        stack_ref: String,
        /// Require the stack ref to be visible to this team.
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
        /// Install target: codex, claude-code, codex-repo, claude-code-repo, or local.
        #[arg(long, value_name = "TARGET")]
        target: String,
        /// Replace existing child installs when safe.
        #[arg(long)]
        force: bool,
    },
    /// Update an installed registry stack in a target runtime.
    #[command(after_help = "Examples:\n  \
              agentstack stack update acme/engineering-default --target codex-repo --check\n  \
              agentstack stack update acme/engineering-default --target codex-repo\n  \
              agentstack stack update acme/engineering-default --target codex-repo --prune")]
    Update {
        /// Installed stack name or ref `stack` or `org/stack`.
        stack_ref: String,
        /// Install target: codex, claude-code, codex-repo, claude-code-repo, or local.
        #[arg(long, value_name = "TARGET")]
        target: String,
        /// Report what would change without writing anything.
        #[arg(long)]
        check: bool,
        /// Re-apply the current stack resolution even when receipts look up to date.
        #[arg(long)]
        force: bool,
        /// Delete skills no longer in the stack definition (without it, they
        /// are detached as standalone installs).
        #[arg(long)]
        prune: bool,
    },
    /// Uninstall a stack from a target.
    #[command(after_help = "Examples:\n  \
              agentstack stack uninstall acme/engineering-default --target codex-repo --dry-run\n  \
              agentstack stack uninstall acme/engineering-default --target codex-repo --yes")]
    Uninstall {
        /// Installed stack name or ref `stack` or `org/stack`.
        stack_ref: String,
        /// Install target: codex, claude-code, codex-repo, claude-code-repo, or local.
        #[arg(long, value_name = "TARGET")]
        target: String,
        /// Continue past children with a missing or unreadable install receipt, leaving those directories in place.
        #[arg(long)]
        force: bool,
        /// Skip the interactive confirmation prompt.
        #[arg(long)]
        yes: bool,
        /// Show what would be removed without deleting files.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show or change stack visibility.
    Visibility {
        #[command(subcommand)]
        action: StackVisibilityCommand,
    },
    /// List audit events for a stack.
    #[command(after_help = "Examples:\n  \
              agentstack stack audit acme/engineering-default\n  \
              agentstack stack audit engineering-default --team platform\n  \
              agentstack stack audit acme/engineering-default --json")]
    Audit {
        /// Stack ref `stack` or `org/stack`.
        stack_ref: String,
        /// Require the stack ref to be visible to this team.
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum StackVisibilityCommand {
    /// Show the current visibility scope.
    Show {
        /// Stack ref `stack` or `org/stack`.
        stack_ref: String,
        /// Require the stack ref to be visible to this team.
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
    },
    /// Change visibility scope without changing stack items.
    Set {
        /// Stack ref `stack` or `org/stack`.
        stack_ref: String,
        /// New visibility scope. Use `--team` only with `--scope team`.
        #[arg(long, value_enum, value_name = "SCOPE")]
        scope: ScopeArg,
        /// Team slug required when `--scope team` is used.
        #[arg(long, value_name = "TEAM")]
        team: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum InstallCommand {
    /// List install receipts.
    List {
        /// Restrict results to one install target.
        #[arg(long, value_name = "TARGET")]
        target: Option<String>,
        /// Receipt kind to list.
        #[arg(long, value_name = "KIND", default_value = "skill", value_parser = ["skill", "stack", "all"])]
        kind: String,
    },
    /// Explain why an installed skill is present in one target.
    #[command(after_help = "Examples:\n  \
              agentstack install why common-review --target codex-repo\n  \
              agentstack install why common-review --target local --json")]
    Why {
        /// Installed skill name.
        skill: String,
        /// Install target: codex, claude-code, codex-repo, claude-code-repo, or local.
        #[arg(long, value_name = "TARGET")]
        target: String,
    },
    /// Batch-update direct skill receipts.
    #[command(after_help = "Examples:\n  \
              agentstack install update --all --target codex-repo --check")]
    Update {
        /// Update every direct skill receipt in the selected target(s).
        #[arg(long, required = true)]
        all: bool,
        /// Install target: codex, claude-code, codex-repo, claude-code-repo, or local.
        #[arg(long, value_name = "TARGET")]
        target: Option<String>,
        /// Report what would change without writing anything.
        #[arg(long)]
        check: bool,
        /// Re-apply the current version even when the receipt looks up to date.
        #[arg(long)]
        force: bool,
    },
    /// Diagnose one install target.
    Doctor {
        /// Install target: codex, claude-code, codex-repo, claude-code-repo, or local.
        #[arg(long, value_name = "TARGET")]
        target: String,
    },
    /// Remove a stale install lock.
    Unlock {
        /// Install target: codex, claude-code, codex-repo, claude-code-repo, or local.
        #[arg(long, value_name = "TARGET")]
        target: String,
        /// Remove a lock even if it still looks recent.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Human login: open a browser OAuth flow, or validate a piped registry token.
    #[command(after_help = "Examples:\n  \
              agentstack auth login\n  \
              agentstack auth login --no-browser\n  \
              printf '%s' \"$TOKEN\" | agentstack auth login\n  \
              agentstack auth login < token.txt\n\n\
              Human path: auth login opens a browser-based OAuth flow and stores the resulting\n\
              AgentStack token in the AgentStack credentials file. The first supported provider is Google.\n\
              Raw token fallback: with piped input or --token-stdin, auth login validates one\n\
              issued AgentStack registry token instead of starting OAuth.\n\
              Machine path: agents, CI, and headless jobs should not run auth login. Set\n\
              AGENTSTACK_REGISTRY_URL plus AGENTSTACK_TOKEN_PATH, or AGENTSTACK_TOKEN for one\n\
              process.\n\
              Stored tokens live in the credential store and are never written to config.toml or printed.")]
    Login {
        /// Explicitly read the bearer token from stdin. A trailing newline is ignored.
        #[arg(long)]
        token_stdin: bool,
        /// OAuth provider to use for browser login.
        #[arg(long, value_enum, value_name = "PROVIDER", default_value_t = OAuthProviderArg::Google)]
        provider: OAuthProviderArg,
        /// Print the authorization URL instead of opening a browser.
        #[arg(long)]
        no_browser: bool,
        /// Loopback callback port. Defaults to 49152.
        #[arg(long, value_name = "PORT")]
        callback_port: Option<u16>,
        /// Seconds to wait for the browser callback.
        #[arg(long, value_name = "SECONDS", default_value_t = 300)]
        timeout_seconds: u64,
    },
    /// Show local auth configuration without calling the registry.
    Status,
    /// Remove the stored registry token.
    Logout,
    /// Identify the active registry token.
    Whoami,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OAuthProviderArg {
    Google,
}

impl OAuthProviderArg {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Google => "google",
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum TargetCommand {
    /// List known install targets.
    List,
    /// Detect installed agent runtimes.
    Detect,
    /// Configure a built-in install target interactively or with --path. Use
    /// this before first installing into user-level targets `claude-code` or
    /// `codex`, or to register any non-default path.
    Setup {
        /// Built-in target name. Use `codex` or `claude-code` for user-level
        /// installs; use `codex-repo` (`repo-codex`) or `claude-code-repo`
        /// (`repo-claude-code`) for the current repo; or use `local`.
        target: Option<String>,
        /// Register this absolute directory for the target without prompting.
        #[arg(long, value_name = "ABSOLUTE_PATH")]
        path: Option<PathBuf>,
        /// Accept the platform default path for the target without prompting.
        #[arg(long)]
        yes: bool,
    },
    /// Print a built-in target's resolved path.
    Path {
        /// Built-in target name. Use `codex` or `claude-code` for user-level
        /// installs; use `codex-repo` (`repo-codex`) or `claude-code-repo`
        /// (`repo-claude-code`) for the current repo; or use `local`.
        target: String,
    },
    /// Set a built-in target path override.
    Set {
        /// Built-in target name. Use `codex` or `claude-code` for user-level
        /// installs; use `codex-repo` (`repo-codex`) or `claude-code-repo`
        /// (`repo-claude-code`) for the current repo; or use `local`.
        target: String,
        #[arg(long, value_name = "PATH")]
        path: PathBuf,
    },
    /// Remove a built-in target path override.
    Unset {
        /// Built-in target name. Use `codex` or `claude-code` for user-level
        /// installs; use `codex-repo` (`repo-codex`) or `claude-code-repo`
        /// (`repo-claude-code`) for the current repo; or use `local`.
        target: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum AuditCommand {
    /// List org audit events.
    #[command(after_help = "Examples:\n  \
              agentstack audit list\n  \
              agentstack audit list --org acme\n  \
              agentstack audit list --org acme --json")]
    List {
        /// Org whose audit trail should be listed. Omit when the active token has one org.
        #[arg(long, value_name = "ORG")]
        org: Option<String>,
    },
    /// Show one org audit event.
    #[command(after_help = "Examples:\n  \
              agentstack audit show aud_123\n  \
              agentstack audit show aud_123 --org acme\n  \
              agentstack audit show aud_123 --org acme --json")]
    Show {
        /// Audit event id returned as `audit_event_id` by registry mutation JSON.
        event_id: String,
        /// Owning org slug for the audit event. Omit when the active token has one org.
        #[arg(long, value_name = "ORG")]
        org: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ScopeArg {
    Private,
    Org,
    Team,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CatalogSortArg {
    Name,
    Updated,
    Owner,
    Installs,
}

impl CatalogSortArg {
    pub const fn as_sort_str(self) -> &'static str {
        match self {
            CatalogSortArg::Name => "name",
            CatalogSortArg::Updated => "updated",
            CatalogSortArg::Owner => "owner",
            CatalogSortArg::Installs => "installs",
        }
    }
}

impl ScopeArg {
    pub const fn as_visibility_str(self) -> &'static str {
        match self {
            ScopeArg::Private => "private",
            ScopeArg::Org => "org",
            ScopeArg::Team => "team",
        }
    }
}

impl std::fmt::Display for ScopeArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_visibility_str())
    }
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the AgentStack config directory path.
    Path,

    /// Print parsed `config.toml` contents.
    Show,
}

#[derive(Debug, Subcommand)]
pub enum RegistryCommand {
    /// Verify the active registry is reachable.
    Ping {
        /// Also validate the active bearer token with the registry.
        #[arg(long)]
        auth: bool,
    },
    /// Save the registry base URL in config.
    Use {
        /// Registry base URL (e.g. `https://registry.agentstack.gg`).
        url: String,
    },
    /// Print the active registry URL.
    Show,
}

#[derive(Debug, Subcommand)]
pub enum TeamsCommand {
    /// Create a new team in an organization.
    Create {
        /// Team ref `org/team`.
        team_ref: String,
    },
    /// List teams in one organization.
    List {
        #[arg(long, value_name = "ORG")]
        org: Option<String>,
    },
    /// Inspect team membership as an org admin or team admin.
    Inspect {
        /// Team ref `org/team`.
        team_ref: String,
    },
    /// Add a user to a team.
    #[command(after_help = "Examples:\n  \
              agentstack team add-member acme/platform user@example.com --role member\n  \
              agentstack team add-member acme/platform admin@example.com --role team_admin")]
    AddMember {
        /// Team ref `org/team`.
        team_ref: String,
        /// User email address to add.
        email: String,
        /// Role: member or team_admin.
        #[arg(long, value_name = "ROLE")]
        role: String,
    },
    /// Remove a user from a team.
    RemoveMember { team_ref: String, email: String },
    /// Update a user's role on a team.
    #[command(after_help = "Examples:\n  \
              agentstack team set-role acme/platform user@example.com --role member\n  \
              agentstack team set-role acme/platform admin@example.com --role team_admin")]
    SetRole {
        /// Team ref `org/team`.
        team_ref: String,
        /// User email address to update.
        email: String,
        /// Role: member or team_admin.
        #[arg(long, value_name = "ROLE")]
        role: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// Print the local package cache path.
    Path,
    /// List cached skill packages.
    List,
    /// Remove all cached packages for one skill.
    Remove {
        /// Skill name to remove.
        name: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        force: bool,
    },
}

/// Shells `agentstack completion` knows how to generate scripts for. Mirrors
/// [`clap_complete::Shell`] but is named so it shows up in `--help` cleanly.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ShellArg {
    Bash,
    Elvish,
    Fish,
    PowerShell,
    Zsh,
}

impl From<ShellArg> for Shell {
    fn from(v: ShellArg) -> Self {
        match v {
            ShellArg::Bash => Shell::Bash,
            ShellArg::Elvish => Shell::Elvish,
            ShellArg::Fish => Shell::Fish,
            ShellArg::PowerShell => Shell::PowerShell,
            ShellArg::Zsh => Shell::Zsh,
        }
    }
}
