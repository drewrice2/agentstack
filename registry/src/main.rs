use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use agentstack_server::{
    AppState, BuildInfo,
    admin::{self, Role, TeamRole, TokenExpiry},
    blob_store::{BlobStore, FsBlobStore},
    build_app,
    config::Config,
    db::{connect_and_migrate, connect_read_only},
    seed,
};
use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "agentstack-server",
    version,
    about = "AgentStack local registry server",
    after_help = "Local demo:\n  agentstack-server dev\n  agentstack-server dev --data-dir /tmp/agentstack-localhost --port 18080\n\nConfigured environment variables:\n  AGENTSTACK_SERVER_BIND=127.0.0.1:8080\n  AGENTSTACK_DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/agentstack\n  AGENTSTACK_BLOB_DIR=/tmp/agentstack-blobs"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Local-only administrative inspection and revocation commands.
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
    /// Initialize an empty Postgres database or verify the existing schema contract.
    InitDb,
    /// Bootstrap users.
    User {
        #[command(subcommand)]
        command: UserCommand,
    },
    /// Bootstrap organizations and memberships.
    Org {
        #[command(subcommand)]
        command: OrgCommand,
    },
    /// Manage bearer tokens.
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
    /// Bootstrap deferred team internals.
    #[command(hide = true)]
    Team {
        #[command(subcommand)]
        command: TeamCommand,
    },
    /// Start a localhost-only demo server with temp-friendly blob storage.
    #[command(
        visible_alias = "serve-local",
        long_about = "Start a localhost-only demo server with filesystem blob storage.\n\nThe server binds only to 127.0.0.1 and uses AGENTSTACK_DATABASE_URL when set. By default, it stores blob data under the OS temp directory. Pass --data-dir to keep blob state in a known disposable location.\n\nExamples:\n  agentstack-server dev\n  agentstack-server dev --data-dir /tmp/agentstack-localhost --port 18080"
    )]
    Dev {
        /// Directory for the local demo blob store.
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Localhost port to bind.
        #[arg(long, default_value_t = 8080)]
        port: u16,
    },
    /// Start the HTTP registry server.
    #[command(
        long_about = "Start the local HTTP registry server.\n\nConfiguration is read from AGENTSTACK_SERVER_BIND, AGENTSTACK_DATABASE_URL, AGENTSTACK_BLOB_DIR, and AGENTSTACK_MAX_* quota environment variables.\n\nLocal example:\n  AGENTSTACK_DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/agentstack AGENTSTACK_BLOB_DIR=/tmp/agentstack-blobs agentstack-server serve\n\nFor a guided localhost-only demo server, run:\n  agentstack-server dev"
    )]
    Serve,
}

#[derive(Debug, Subcommand)]
enum TeamCommand {
    /// Create a team in an organization.
    Create {
        /// Organization slug.
        org: String,
        /// Team slug.
        team: String,
        /// Email of the initial team admin.
        #[arg(long = "team-admin", alias = "lead")]
        team_admin: String,
    },
    /// Add a user to a team with a role.
    AddMember {
        /// Organization slug.
        org: String,
        /// Team slug.
        team: String,
        /// User email.
        user_email: String,
        /// Team role.
        #[arg(long)]
        role: TeamRoleArg,
    },
    /// Remove a user from a team.
    RemoveMember {
        org: String,
        team: String,
        user_email: String,
    },
    /// Update a user's role on a team.
    SetRole {
        org: String,
        team: String,
        user_email: String,
        #[arg(long)]
        role: TeamRoleArg,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
#[value(rename_all = "snake_case")]
enum TeamRoleArg {
    Member,
    #[value(alias = "lead")]
    TeamAdmin,
}

impl From<TeamRoleArg> for TeamRole {
    fn from(value: TeamRoleArg) -> Self {
        match value {
            TeamRoleArg::Member => TeamRole::Member,
            TeamRoleArg::TeamAdmin => TeamRole::TeamAdmin,
        }
    }
}

#[derive(Debug, Subcommand)]
enum AdminCommand {
    /// Inspect or revoke bearer tokens in the configured database.
    Tokens {
        #[command(subcommand)]
        command: AdminTokensCommand,
    },
    /// Inspect users in the configured database.
    Users {
        #[command(subcommand)]
        command: AdminUsersCommand,
    },
    /// Inspect audit events without starting HTTP.
    Audit {
        #[command(subcommand)]
        command: AdminAuditCommand,
    },
}

#[derive(Debug, Subcommand)]
enum AdminTokensCommand {
    /// List tokens without revealing raw token values.
    List,
    /// Revoke one token by id.
    Revoke {
        /// Token id printed by `token issue` or `admin tokens list`.
        id: String,
    },
}

#[derive(Debug, Subcommand)]
enum AdminUsersCommand {
    /// List users.
    List,
    /// Grant the global server-admin flag to a user.
    SetAdmin {
        /// User email address.
        email: String,
    },
    /// Remove the global server-admin flag from a user.
    UnsetAdmin {
        /// User email address.
        email: String,
    },
}

#[derive(Debug, Subcommand)]
enum AdminAuditCommand {
    /// List audit events for one organization.
    List {
        /// Organization slug.
        #[arg(long)]
        org: String,
        /// Maximum events to return.
        #[arg(long, default_value_t = 50, value_parser = parse_audit_limit)]
        limit: u16,
        /// Print JSON instead of compact TSV.
        #[arg(long)]
        json: bool,
    },
    /// Show one audit event for one organization.
    Show {
        /// Organization slug.
        #[arg(long)]
        org: String,
        /// Audit event id.
        event_id: String,
        /// Print JSON instead of compact TSV.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum UserCommand {
    /// Create a user account.
    Create {
        /// User email address.
        email: String,
        /// Optional display name.
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum OrgCommand {
    /// Create an organization.
    Create {
        /// Organization slug.
        slug: String,
        /// Optional display name.
        #[arg(long)]
        name: Option<String>,
        /// Owner email for local provisioning. Seeds canonical agentstack@1.
        #[arg(long)]
        owner_email: Option<String>,
        /// Optional display name for a newly-created owner user.
        #[arg(long, requires = "owner_email")]
        owner_name: Option<String>,
    },
    /// Grant a role to a user in an organization.
    Grant {
        /// Organization slug.
        org: String,
        /// User email address.
        user_email: String,
        /// Role to grant.
        #[arg(long)]
        role: RoleArg,
    },
}

#[derive(Debug, Subcommand)]
enum TokenCommand {
    /// Issue a bearer token. The raw token is printed once.
    ///
    /// Tokens expire after `--expires-in-days` (default 30) so leaked tokens
    /// self-expire. Pass `--no-expiry` to opt out for local/admin scenarios.
    Issue {
        /// User email address.
        user_email: String,
        /// Human-readable token label.
        #[arg(long)]
        label: String,
        /// Token lifetime in days. Defaults to 30. Conflicts with --no-expiry.
        #[arg(
            long,
            conflicts_with = "no_expiry",
            default_value_t = TokenExpiry::DEFAULT_TTL_DAYS,
            value_parser = parse_positive_days,
        )]
        expires_in_days: u32,
        /// Issue a token without an expiry (admin/local override).
        #[arg(long)]
        no_expiry: bool,
        /// Print only the raw token followed by one newline.
        #[arg(long)]
        raw: bool,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
#[value(rename_all = "snake_case")]
enum RoleArg {
    OrgAdmin,
    Publisher,
    Reader,
}

fn parse_positive_days(value: &str) -> Result<u32, String> {
    let days = value
        .parse::<u32>()
        .map_err(|_| "must be a positive whole number of days".to_string())?;
    if days == 0 {
        return Err("must be at least 1 day".to_string());
    }
    if days > i32::MAX as u32 {
        return Err(format!("must be at most {} days", i32::MAX));
    }
    Ok(days)
}

fn parse_audit_limit(value: &str) -> Result<u16, String> {
    let limit = value
        .parse::<u16>()
        .map_err(|_| "must be a positive whole number".to_string())?;
    if limit == 0 {
        return Err("must be at least 1".to_string());
    }
    Ok(limit)
}

impl From<RoleArg> for Role {
    fn from(value: RoleArg) -> Self {
        match value {
            RoleArg::OrgAdmin => Role::OrgAdmin,
            RoleArg::Publisher => Role::Publisher,
            RoleArg::Reader => Role::Reader,
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_ansi(false)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Admin { command } => admin_command(command).await,
        Command::InitDb => init_db().await,
        Command::User { command } => user(command).await,
        Command::Org { command } => org(command).await,
        Command::Token { command } => token(command).await,
        Command::Team { command } => team(command).await,
        Command::Dev { data_dir, port } => dev(data_dir, port).await,
        Command::Serve => serve().await,
    }
}

async fn admin_command(command: AdminCommand) -> anyhow::Result<()> {
    let config = Config::from_env()?;
    if let AdminCommand::Audit { command } = command {
        let pool = connect_read_only(&config.database_url)
            .await
            .context("failed to open read-only database connection")?;
        match command {
            AdminAuditCommand::List { org, limit, json } => {
                let events = agentstack_server::audit::list_events(
                    &pool,
                    &org,
                    None,
                    None,
                    Some(limit.into()),
                )
                .await
                .context("failed to list audit events")?;
                if json {
                    serde_json::to_writer_pretty(std::io::stdout(), &events)?;
                    println!();
                } else {
                    print_audit_list_tsv(&events);
                }
            }
            AdminAuditCommand::Show {
                org,
                event_id,
                json,
            } => {
                let event = agentstack_server::audit::show_event(&pool, &org, &event_id)
                    .await
                    .context("failed to load audit event")?
                    .with_context(|| format!("no such audit event `{event_id}` in org `{org}`"))?;
                if json {
                    serde_json::to_writer_pretty(std::io::stdout(), &event)?;
                    println!();
                } else {
                    print_audit_show_tsv(&event)?;
                }
            }
        }
        pool.close().await;
        return Ok(());
    }

    let pool = connect_and_migrate(&config.database_url)
        .await
        .context("failed to initialize database")?;

    match command {
        AdminCommand::Tokens { command } => match command {
            AdminTokensCommand::List => {
                let tokens = admin::list_tokens(&pool).await?;
                if tokens.is_empty() {
                    println!("no tokens");
                } else {
                    for token in tokens {
                        println!(
                            "{}\t{}\t{}\tcreated={}\texpires={}\tlast_used={}\trevoked={}",
                            token.id,
                            token.user_email,
                            token.label,
                            token.created_at,
                            token.expires_at.as_deref().unwrap_or("-"),
                            token.last_used_at.as_deref().unwrap_or("-"),
                            token.revoked_at.as_deref().unwrap_or("-")
                        );
                    }
                }
            }
            AdminTokensCommand::Revoke { id } => {
                let token = admin::revoke_token(&pool, &id).await?;
                println!("revoked token {}", token.id);
                println!("user: {}", token.user_email);
                println!("label: {}", token.label);
                if let Some(revoked_at) = token.revoked_at {
                    println!("revoked_at: {revoked_at}");
                }
            }
        },
        AdminCommand::Users { command } => match command {
            AdminUsersCommand::List => {
                let users = admin::list_users(&pool).await?;
                if users.is_empty() {
                    println!("no users");
                } else {
                    for user in users {
                        println!(
                            "{}\t{}\tadmin={}\tname={}\tcreated={}\tupdated={}",
                            user.id,
                            user.email,
                            user.is_server_admin,
                            user.name.as_deref().unwrap_or("-"),
                            user.created_at,
                            user.updated_at
                        );
                    }
                }
            }
            AdminUsersCommand::SetAdmin { email } => {
                let user = admin::set_server_admin(&pool, &email, true).await?;
                println!("set server-admin flag for {}", user.email);
            }
            AdminUsersCommand::UnsetAdmin { email } => {
                let user = admin::set_server_admin(&pool, &email, false).await?;
                println!("unset server-admin flag for {}", user.email);
            }
        },
        AdminCommand::Audit { .. } => unreachable!("audit handled with read-only connection"),
    }

    pool.close().await;
    Ok(())
}

async fn init_db() -> anyhow::Result<()> {
    let config = Config::from_env()?;
    let pool = connect_and_migrate(&config.database_url)
        .await
        .context("failed to initialize database")?;
    pool.close().await;
    println!("database initialized");
    Ok(())
}

fn print_audit_list_tsv(events: &[agentstack_server::audit::AuditEvent]) {
    for event in events {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            event.id,
            event.created_at,
            event.action,
            event.resource_type,
            event.resource.as_deref().unwrap_or("-"),
            event.actor_email.as_deref().unwrap_or("-")
        );
    }
}

fn print_audit_show_tsv(event: &agentstack_server::audit::AuditEvent) -> anyhow::Result<()> {
    println!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}",
        event.id,
        event.created_at,
        event.action,
        event.resource_type,
        event.resource.as_deref().unwrap_or("-"),
        event.actor_email.as_deref().unwrap_or("-"),
        serde_json::to_string(&event.metadata)?
    );
    Ok(())
}

async fn user(command: UserCommand) -> anyhow::Result<()> {
    let config = Config::from_env()?;
    let pool = connect_and_migrate(&config.database_url)
        .await
        .context("failed to initialize database")?;

    match command {
        UserCommand::Create { email, name } => {
            let user = admin::create_user(&pool, &email, name.as_deref()).await?;
            println!("created user {}", user.email);
            println!("id: {}", user.id);
            if let Some(name) = user.name {
                println!("name: {name}");
            }
        }
    }

    pool.close().await;
    Ok(())
}

async fn org(command: OrgCommand) -> anyhow::Result<()> {
    let config = Config::from_env()?;
    let pool = connect_and_migrate(&config.database_url)
        .await
        .context("failed to initialize database")?;

    match command {
        OrgCommand::Create {
            slug,
            name,
            owner_email,
            owner_name,
        } => {
            if let Some(owner_email) = owner_email {
                let blob_store = build_blob_store(config.blob_dir.clone()).await?;
                let provisioned = seed::provision_org_with_owner(
                    &pool,
                    blob_store.as_ref(),
                    &config.quotas,
                    &slug,
                    name.as_deref(),
                    &owner_email,
                    owner_name.as_deref(),
                )
                .await?;
                println!("created org {}", provisioned.org_slug);
                println!("id: {}", provisioned.org_id);
                println!("name: {}", provisioned.org_name);
                println!("created owner user {}", provisioned.owner_email);
                println!("granted org_admin to {}", provisioned.owner_email);
                if provisioned.seed_created {
                    println!(
                        "seeded {}/agentstack@1 ({})",
                        provisioned.org_slug, provisioned.seed_archive_hash
                    );
                } else {
                    println!(
                        "seed already present: {}/agentstack@1",
                        provisioned.org_slug
                    );
                }
            } else {
                let org = admin::create_org(&pool, &slug, name.as_deref()).await?;
                println!("created org {}", org.slug);
                println!("id: {}", org.id);
                println!("name: {}", org.name);
            }
        }
        OrgCommand::Grant {
            org,
            user_email,
            role,
        } => {
            let grant = admin::grant_org_role_with_quotas(
                &pool,
                &config.quotas,
                &org,
                &user_email,
                role.into(),
            )
            .await?;
            println!(
                "granted {} to {} in {}",
                grant.role.as_str(),
                grant.user_email,
                grant.org_slug
            );
        }
    }

    pool.close().await;
    Ok(())
}

async fn token(command: TokenCommand) -> anyhow::Result<()> {
    let config = Config::from_env()?;
    let pool = connect_and_migrate(&config.database_url)
        .await
        .context("failed to initialize database")?;

    match command {
        TokenCommand::Issue {
            user_email,
            label,
            expires_in_days,
            no_expiry,
            raw,
        } => {
            let expiry = if no_expiry {
                TokenExpiry::Indefinite
            } else {
                TokenExpiry::Days(expires_in_days)
            };
            let issued =
                admin::issue_token_with_quotas(&pool, &config.quotas, &user_email, &label, expiry)
                    .await?;
            if raw {
                println!("{}", issued.raw_token);
            } else {
                println!("created token {} for {}", issued.label, issued.user_email);
                println!("token_id: {}", issued.token_id);
                match expiry {
                    TokenExpiry::Days(days) => println!("expires_in_days: {days}"),
                    TokenExpiry::Indefinite => println!("expires_in_days: never"),
                }
                println!("token: {}", issued.raw_token);
            }
        }
    }

    pool.close().await;
    Ok(())
}

async fn team(command: TeamCommand) -> anyhow::Result<()> {
    let config = Config::from_env()?;
    let pool = connect_and_migrate(&config.database_url)
        .await
        .context("failed to initialize database")?;

    match command {
        TeamCommand::Create {
            org,
            team,
            team_admin,
        } => {
            let record =
                admin::create_team_with_quotas(&pool, &config.quotas, &org, &team, &team_admin)
                    .await?;
            println!("created team {}/{}", record.org_slug, record.slug);
        }
        TeamCommand::AddMember {
            org,
            team,
            user_email,
            role,
        } => {
            let record = admin::add_team_member_with_quotas(
                &pool,
                &config.quotas,
                &org,
                &team,
                &user_email,
                role.into(),
            )
            .await?;
            println!(
                "added {} to {}/{} as {}",
                record.email,
                org,
                team,
                record.role.as_str()
            );
        }
        TeamCommand::RemoveMember {
            org,
            team,
            user_email,
        } => {
            admin::remove_team_member(&pool, &org, &team, &user_email).await?;
            println!("removed {user_email} from {org}/{team}");
        }
        TeamCommand::SetRole {
            org,
            team,
            user_email,
            role,
        } => {
            let record = admin::set_team_role(&pool, &org, &team, &user_email, role.into()).await?;
            println!(
                "set {} on {}/{} to {}",
                record.email,
                org,
                team,
                record.role.as_str()
            );
        }
    }

    pool.close().await;
    Ok(())
}

async fn serve() -> anyhow::Result<()> {
    let config = Config::from_env()?;
    run_server(config, None).await
}

async fn dev(data_dir: Option<PathBuf>, port: u16) -> anyhow::Result<()> {
    let data_dir = data_dir.unwrap_or_else(default_dev_data_dir);
    tokio::fs::create_dir_all(&data_dir)
        .await
        .with_context(|| format!("failed to create data directory `{}`", data_dir.display()))?;
    let data_dir = tokio::fs::canonicalize(&data_dir).await.unwrap_or(data_dir);
    let blob_dir = data_dir.join("blobs");
    let env_config = Config::from_env()?;
    let database_url = env_config.database_url.clone();
    let config = Config {
        bind: SocketAddr::from(([127, 0, 0, 1], port)),
        database_url: database_url.clone(),
        blob_dir: blob_dir.clone(),
        quotas: env_config.quotas,
    };
    run_server(
        config,
        Some(DevStartup {
            database_url,
            blob_dir,
        }),
    )
    .await
}

fn default_dev_data_dir() -> PathBuf {
    std::env::temp_dir().join("agentstack-localhost")
}

struct DevStartup {
    database_url: String,
    blob_dir: PathBuf,
}

async fn run_server(config: Config, dev_startup: Option<DevStartup>) -> anyhow::Result<()> {
    let bind = config.bind;
    let database_url = config.database_url;
    let blob_dir = config.blob_dir;
    let quotas = config.quotas;

    let db = connect_and_migrate(&database_url)
        .await
        .context("failed to initialize database")?;
    let blob_store = build_blob_store(blob_dir).await?;
    let state = AppState::new(db, blob_store, env!("CARGO_PKG_VERSION"))
        .with_build_info(BuildInfo::from_env())
        .with_quotas(quotas);
    let app = build_app(state);

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .context("failed to bind server socket")?;
    let actual_addr = listener
        .local_addr()
        .context("failed to read server socket address")?;
    tracing::info!(bind = %actual_addr, "agentstack-server listening");
    if let Some(startup) = dev_startup {
        print_dev_startup(&startup, actual_addr);
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server failed")
}

async fn build_blob_store(blob_dir: PathBuf) -> anyhow::Result<Arc<dyn BlobStore>> {
    ensure_blob_dir(&blob_dir).await?;
    Ok(Arc::new(FsBlobStore::new(blob_dir)))
}

fn print_dev_startup(startup: &DevStartup, bind: SocketAddr) {
    let url = format!("http://{bind}");
    println!("AgentStack local registry");
    println!("  server:   {url}");
    println!("  database: {}", startup.database_url);
    println!("  blobs:    {}", startup.blob_dir.display());
    println!();
    println!("Use these env vars for bootstrap commands in another terminal:");
    println!(
        "  export AGENTSTACK_DATABASE_URL={}",
        shell_quote(&startup.database_url)
    );
    println!(
        "  export AGENTSTACK_BLOB_DIR={}",
        shell_quote(&startup.blob_dir.display().to_string())
    );
    println!();
    println!("Next bootstrap step:");
    println!(
        "  agentstack-server org create pilot --name \"Pilot\" --owner-email pilot@example.com --owner-name \"Pilot Admin\""
    );
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

async fn ensure_blob_dir(blob_dir: &std::path::Path) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(blob_dir)
        .await
        .with_context(|| format!("failed to create blob directory `{}`", blob_dir.display()))?;
    let metadata = tokio::fs::metadata(blob_dir)
        .await
        .with_context(|| format!("failed to inspect blob directory `{}`", blob_dir.display()))?;
    if !metadata.is_dir() {
        anyhow::bail!("blob path `{}` is not a directory", blob_dir.display());
    }
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
