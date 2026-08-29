//! Auth-token storage.
//!
//! Three pieces:
//!
//! 1. [`Token`] — a newtype that **redacts itself** on `Debug`/`Display`. The
//!    raw value is only available through [`Token::expose_secret`] so it
//!    can't accidentally leak into logs or error chains.
//! 2. [`TokenStore`] — trait abstracting the backing store. Production code
//!    defaults to a config-directory [`FileTokenStore`] with mode `0600`;
//!    tests use [`MemoryTokenStore`]. The OS keyring is available by setting
//!    `AGENTSTACK_CREDENTIAL_STORE=keychain`.
//! 3. [`resolve_token`] — single entry point that checks the
//!    `AGENTSTACK_TOKEN` env var first (CI override), then
//!    `AGENTSTACK_TOKEN_PATH` (read-only secret file), and falls back to the
//!    configured store.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once};

use anyhow::{Context, Result, anyhow};

use crate::registry::RegistryUrl;

/// Service identifier used for keyring entries.
const SERVICE: &str = "agentstack";

/// Stable account identifier used when no specific user is provided. The
/// account becomes meaningful once orgs/teams ship.
pub const DEFAULT_ACCOUNT: &str = "default";

/// CI-friendly override: when set and non-empty, this value wins over any
/// token stored in the credential store.
pub const TOKEN_ENV: &str = "AGENTSTACK_TOKEN";

/// Headless-friendly override: when set and non-empty, read one bearer token
/// from this file path. The file is never written by AgentStack.
pub const TOKEN_PATH_ENV: &str = "AGENTSTACK_TOKEN_PATH";

const TOKEN_PATH_MAX_BYTES: u64 = 8 * 1024;

/// Test-only override: when set and non-empty, switches the default
/// [`token_store`] to a plaintext file-backed store at the given path.
/// This exists so integration tests can exercise the full login/logout
/// path without touching the real keyring; **never** point a production
/// install at it. Honored only in debug builds or when
/// [`ALLOW_TOKEN_FILE_ENV`] is set to `"1"`.
pub const TOKEN_FILE_ENV: &str = "AGENTSTACK_TOKEN_FILE";

/// Opt-in for [`TOKEN_FILE_ENV`] outside of debug builds. Setting this to
/// `"1"` acknowledges that the file-backed token store is plaintext and
/// test-only.
pub const ALLOW_TOKEN_FILE_ENV: &str = "AGENTSTACK_ALLOW_TOKEN_FILE";

/// Human credential-store selector. Supported values: `file` (default) and
/// `keychain`.
pub const CREDENTIAL_STORE_ENV: &str = "AGENTSTACK_CREDENTIAL_STORE";

const DEFAULT_CREDENTIALS_FILE_NAME: &str = "credentials.json";

/// Build the token-store account for one logical account at one registry.
pub fn scoped_account(registry_url: &str, account: &str) -> Result<String> {
    let registry = RegistryUrl::parse(registry_url)?;
    Ok(format!(
        "registry:{}:account:{account}",
        registry.normalized_base()
    ))
}

/// Where a token came from, so callers can show provenance to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    /// Pulled from the `AGENTSTACK_TOKEN` env var.
    Env,
    /// Pulled from the read-only `AGENTSTACK_TOKEN_PATH` file.
    Path,
    /// Pulled from the configured [`TokenStore`].
    Store,
}

/// A secret bearer token. Redacts itself on `Debug` and `Display`; the raw
/// value is only reachable via [`Token::expose_secret`].
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn from_material(label: &str, value: impl AsRef<str>) -> Result<Self> {
        normalize_token_material(label, value.as_ref()).map(Self)
    }

    /// Returns the underlying secret. Use sparingly and never log the result.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    /// Redacted view safe to print: shows the last 4 chars when long enough,
    /// otherwise `"***"`.
    pub fn redacted(&self) -> String {
        redact(&self.0)
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Token").field(&self.redacted()).finish()
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.redacted())
    }
}

/// Redact a token-shaped string for display. Reveals the last four
/// characters when the input is long enough to make that safe; otherwise
/// emits `"***"` so nothing leaks.
pub fn redact(token: &str) -> String {
    let n = token.chars().count();
    if n == 0 {
        return "<empty>".into();
    }
    if n <= 8 {
        return "***".into();
    }
    let last4: String = token
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("***{last4}")
}

/// Trim surrounding ASCII whitespace and validate bearer token material.
///
/// RFC 6750 bearer credentials use `b64token`: ASCII letters/digits plus
/// `-._~+/`, with optional `=` padding at the end.
pub fn normalize_token_material(label: &str, raw: &str) -> Result<String> {
    let token = raw.trim_matches(|c: char| c.is_ascii_whitespace());
    if token.is_empty() {
        bail_token(label, "must not be empty")?;
    }
    if token.chars().any(|c| c.is_ascii_whitespace()) {
        bail_token(label, "must not contain internal whitespace")?;
    }
    if token.chars().any(|c| c.is_control()) {
        bail_token(label, "must not contain control characters")?;
    }
    if !token.is_ascii() {
        bail_token(label, "must contain only ASCII characters")?;
    }

    let mut seen_base = false;
    let mut seen_padding = false;
    for c in token.chars() {
        if c == '=' {
            seen_padding = true;
            continue;
        }
        seen_base = true;
        if seen_padding || !is_rfc6750_bearer_char(c) {
            bail_token(label, "contains invalid RFC 6750 bearer token characters")?;
        }
    }
    if !seen_base {
        bail_token(label, "must contain at least one bearer token character")?;
    }

    Ok(token.to_string())
}

fn bail_token(label: &str, reason: &str) -> Result<()> {
    Err(anyhow!("{label} {reason}"))
}

fn is_rfc6750_bearer_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~' | '+' | '/')
}

/// Backing storage for auth tokens.
pub trait TokenStore: Send + Sync {
    fn save(&self, account: &str, token: &Token) -> Result<()>;
    fn load(&self, account: &str) -> Result<Option<Token>>;
    fn delete(&self, account: &str) -> Result<()>;
    /// Short, human-readable name of the backing store ("keyring", "memory", "file").
    fn kind(&self) -> &'static str;
}

/// OS-native keyring store. Opt in with `AGENTSTACK_CREDENTIAL_STORE=keychain`.
#[derive(Debug, Default)]
pub struct KeyringTokenStore;

impl TokenStore for KeyringTokenStore {
    fn save(&self, account: &str, token: &Token) -> Result<()> {
        let entry =
            keyring::Entry::new(SERVICE, account).context("failed to open keyring entry")?;
        entry
            .set_password(token.expose_secret())
            .context("failed to write token to keyring")
    }

    fn load(&self, account: &str) -> Result<Option<Token>> {
        let entry =
            keyring::Entry::new(SERVICE, account).context("failed to open keyring entry")?;
        match entry.get_password() {
            Ok(t) => Ok(Some(Token::new(t))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) if is_missing_keychain(&e) => Ok(None),
            Err(e) => Err(e).context("failed to read token from keyring"),
        }
    }

    fn delete(&self, account: &str) -> Result<()> {
        let entry =
            keyring::Entry::new(SERVICE, account).context("failed to open keyring entry")?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e).context("failed to delete token from keyring"),
        }
    }

    fn kind(&self) -> &'static str {
        "keyring"
    }
}

fn is_missing_keychain(error: &keyring::Error) -> bool {
    match error {
        keyring::Error::PlatformFailure(source) | keyring::Error::NoStorageAccess(source) => source
            .to_string()
            .to_ascii_lowercase()
            .contains("default keychain could not be found"),
        _ => false,
    }
}

/// In-memory store for unit tests. Never persisted, never leaks.
#[derive(Debug, Default)]
pub struct MemoryTokenStore {
    tokens: Mutex<HashMap<String, String>>,
}

impl MemoryTokenStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl TokenStore for MemoryTokenStore {
    fn save(&self, account: &str, token: &Token) -> Result<()> {
        self.tokens
            .lock()
            .unwrap()
            .insert(account.to_string(), token.expose_secret().to_string());
        Ok(())
    }

    fn load(&self, account: &str) -> Result<Option<Token>> {
        Ok(self
            .tokens
            .lock()
            .unwrap()
            .get(account)
            .cloned()
            .map(Token::new))
    }

    fn delete(&self, account: &str) -> Result<()> {
        self.tokens.lock().unwrap().remove(account);
        Ok(())
    }

    fn kind(&self) -> &'static str {
        "memory"
    }
}

/// File-backed plaintext store. Format is a tiny JSON map of
/// `account -> token`; writes use mode `0600`.
#[derive(Debug)]
pub struct FileTokenStore {
    path: PathBuf,
}

impl FileTokenStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn read_map(&self) -> Result<HashMap<String, String>> {
        if !self.path.exists() {
            return Ok(HashMap::new());
        }
        let text = fs::read_to_string(&self.path)
            .with_context(|| format!("failed to read `{}`", self.path.display()))?;
        if text.trim().is_empty() {
            return Ok(HashMap::new());
        }
        let map: HashMap<String, String> = serde_json::from_str(&text)
            .with_context(|| format!("failed to parse `{}`", self.path.display()))?;
        Ok(map)
    }

    fn write_map(&self, map: &HashMap<String, String>) -> Result<()> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            if !parent.exists() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create `{}`", parent.display()))?;
            }
            #[cfg(unix)]
            if self.path.file_name().and_then(|name| name.to_str())
                == Some(DEFAULT_CREDENTIALS_FILE_NAME)
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                    .with_context(|| format!("failed to secure `{}`", parent.display()))?;
            }
        }
        let text = serde_json::to_string(map).context("failed to serialize token store")?;
        crate::fs_atomic::write_string_with_mode(&self.path, &text, 0o600)
            .with_context(|| format!("failed to write `{}`", self.path.display()))?;
        Ok(())
    }
}

impl TokenStore for FileTokenStore {
    fn save(&self, account: &str, token: &Token) -> Result<()> {
        let mut map = self.read_map()?;
        map.insert(account.to_string(), token.expose_secret().to_string());
        self.write_map(&map)
    }

    fn load(&self, account: &str) -> Result<Option<Token>> {
        let map = self.read_map()?;
        Ok(map.get(account).cloned().map(Token::new))
    }

    fn delete(&self, account: &str) -> Result<()> {
        let mut map = self.read_map()?;
        if map.remove(account).is_some() {
            self.write_map(&map)?;
        }
        Ok(())
    }

    fn kind(&self) -> &'static str {
        "file"
    }
}

/// Store that reports a construction-time error on every operation. This keeps
/// `token_store()` infallible while preserving the real failure at use sites.
#[derive(Debug)]
struct ErrorTokenStore {
    kind: &'static str,
    message: String,
}

impl TokenStore for ErrorTokenStore {
    fn save(&self, _account: &str, _token: &Token) -> Result<()> {
        Err(anyhow!("{}", self.message))
    }

    fn load(&self, _account: &str) -> Result<Option<Token>> {
        Err(anyhow!("{}", self.message))
    }

    fn delete(&self, _account: &str) -> Result<()> {
        Err(anyhow!("{}", self.message))
    }

    fn kind(&self) -> &'static str {
        self.kind
    }
}

/// A token store that always errors. Used when `AGENTSTACK_TOKEN_FILE` is
/// set in a release build without the matching opt-in, so we refuse the
/// test plaintext file store rather than silently falling back to another
/// store.
#[derive(Debug, Default)]
struct RefusedTokenFileStore;

fn refused_token_file_error() -> anyhow::Error {
    anyhow!(
        "{TOKEN_FILE_ENV} is test-only; set {ALLOW_TOKEN_FILE_ENV}=1 (or use a debug build) to opt in, or unset {TOKEN_FILE_ENV}",
    )
}

impl TokenStore for RefusedTokenFileStore {
    fn save(&self, _account: &str, _token: &Token) -> Result<()> {
        Err(refused_token_file_error())
    }

    fn load(&self, _account: &str) -> Result<Option<Token>> {
        Err(refused_token_file_error())
    }

    fn delete(&self, _account: &str) -> Result<()> {
        Err(refused_token_file_error())
    }

    fn kind(&self) -> &'static str {
        "refused"
    }
}

/// True when `AGENTSTACK_TOKEN_FILE` is allowed: either this is a debug
/// build, or the operator opted in via `AGENTSTACK_ALLOW_TOKEN_FILE=1`.
pub fn token_file_allowed() -> bool {
    cfg!(debug_assertions)
        || std::env::var_os(ALLOW_TOKEN_FILE_ENV)
            .map(|v| v == "1")
            .unwrap_or(false)
}

static FILE_STORE_WARNED: Once = Once::new();

fn warn_token_file_once() {
    if std::env::args_os().any(|arg| arg == "--json") {
        return;
    }
    FILE_STORE_WARNED.call_once(|| {
        // This layer is intentionally below command Ctx. Keep the test-only
        // plaintext-store warning as a security diagnostic for human output,
        // but do not pollute JSON mode stderr.
        eprintln!(
            "warning: {TOKEN_FILE_ENV} is honored — plaintext token store is for tests only; do not use in production",
        );
    });
}

/// Default store the CLI uses. Honors `AGENTSTACK_TOKEN_FILE` for tests
/// (debug builds or `AGENTSTACK_ALLOW_TOKEN_FILE=1`); otherwise defaults to
/// `credentials.json` in the AgentStack config directory. Set
/// `AGENTSTACK_CREDENTIAL_STORE=keychain` to opt in to the OS keyring.
pub fn token_store() -> Box<dyn TokenStore> {
    if let Some(path) = token_file_override() {
        if token_file_allowed() {
            warn_token_file_once();
            return Box::new(FileTokenStore::new(path));
        }
        return Box::new(RefusedTokenFileStore);
    }
    match credential_store_kind() {
        CredentialStoreKind::File => match default_credentials_file() {
            Ok(path) => Box::new(FileTokenStore::new(path)),
            Err(err) => Box::new(ErrorTokenStore {
                kind: "file",
                message: format!("failed to resolve credentials file: {err}"),
            }),
        },
        CredentialStoreKind::Keychain => Box::new(KeyringTokenStore),
        CredentialStoreKind::Invalid(raw) => Box::new(ErrorTokenStore {
            kind: "invalid",
            message: format!("{CREDENTIAL_STORE_ENV} must be `file` or `keychain`, got `{raw}`"),
        }),
    }
}

enum CredentialStoreKind {
    File,
    Keychain,
    Invalid(String),
}

fn credential_store_kind() -> CredentialStoreKind {
    match std::env::var(CREDENTIAL_STORE_ENV) {
        Ok(raw) if raw.eq_ignore_ascii_case("keychain") => CredentialStoreKind::Keychain,
        Ok(raw) if raw.eq_ignore_ascii_case("file") || raw.trim().is_empty() => {
            CredentialStoreKind::File
        }
        Ok(raw) => CredentialStoreKind::Invalid(raw),
        Err(_) => CredentialStoreKind::File,
    }
}

pub fn default_credentials_file() -> Result<PathBuf> {
    Ok(crate::config::config_dir()?.join(DEFAULT_CREDENTIALS_FILE_NAME))
}

/// Look up a token, preferring `AGENTSTACK_TOKEN`, then the read-only
/// `AGENTSTACK_TOKEN_PATH` file. Returns `Ok(None)` when the user is not
/// logged in and no override is set.
pub fn resolve_token(
    store: &dyn TokenStore,
    account: &str,
) -> Result<Option<(Token, TokenSource)>> {
    if let Some(raw) = std::env::var_os(TOKEN_ENV) {
        let s = raw.to_string_lossy().into_owned();
        return Ok(Some((
            Token::from_material(TOKEN_ENV, &s)?,
            TokenSource::Env,
        )));
    }
    if let Some(path) = token_path_override() {
        return Ok(Some((read_token_path(&path)?, TokenSource::Path)));
    }
    let stored = store
        .load(account)
        .with_context(|| format!("failed to read {} store", store.kind()))?;
    stored
        .map(|token| {
            Token::from_material("stored token", token.expose_secret())
                .map(|token| (token, TokenSource::Store))
        })
        .transpose()
}

/// True when the `AGENTSTACK_TOKEN` env var is set to a non-empty value.
pub fn env_token_present() -> bool {
    std::env::var_os(TOKEN_ENV)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

/// True when `AGENTSTACK_TOKEN_PATH` is set to a non-empty value.
pub fn env_token_path_present() -> bool {
    std::env::var_os(TOKEN_PATH_ENV)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

/// Path to a read-only bearer-token file, if `AGENTSTACK_TOKEN_PATH` is set.
pub fn token_path_override() -> Option<PathBuf> {
    std::env::var_os(TOKEN_PATH_ENV).and_then(|v| {
        if v.is_empty() {
            None
        } else {
            Some(PathBuf::from(v))
        }
    })
}

fn read_token_path(path: &Path) -> Result<Token> {
    let meta = fs::metadata(path).with_context(|| {
        format!(
            "failed to inspect {TOKEN_PATH_ENV} file `{}`",
            path.display()
        )
    })?;
    if !meta.is_file() {
        return Err(anyhow!(
            "{TOKEN_PATH_ENV} file `{}` is not a regular file",
            path.display()
        ));
    }
    if meta.len() > TOKEN_PATH_MAX_BYTES {
        return Err(anyhow!(
            "{TOKEN_PATH_ENV} file `{}` is too large (max {TOKEN_PATH_MAX_BYTES} bytes)",
            path.display()
        ));
    }
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read {TOKEN_PATH_ENV} file `{}`", path.display()))?;
    Token::from_material(TOKEN_PATH_ENV, &text)
}

/// Path the file-backed store would use, if `AGENTSTACK_TOKEN_FILE` is set.
pub fn token_file_override() -> Option<PathBuf> {
    std::env::var_os(TOKEN_FILE_ENV).and_then(|v| {
        if v.is_empty() {
            None
        } else {
            Some(PathBuf::from(v))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn unique_path(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "agentstack-token-{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn redact_short_token_hides_everything() {
        assert_eq!(redact(""), "<empty>");
        assert_eq!(redact("abc"), "***");
        assert_eq!(redact("12345678"), "***");
    }

    #[test]
    fn redact_long_token_shows_last_four() {
        assert_eq!(redact("supersecretvalue1234"), "***1234");
        assert_eq!(redact("aaaaaaaaaXYZ"), "***aXYZ");
    }

    #[test]
    fn token_display_and_debug_are_redacted() {
        let t = Token::new("supersecretvalue1234");
        assert_eq!(format!("{t}"), "***1234");
        let dbg = format!("{t:?}");
        assert!(dbg.contains("***1234"));
        assert!(!dbg.contains("supersecret"));
    }

    #[test]
    fn token_expose_secret_returns_raw_value() {
        let t = Token::new("supersecretvalue1234");
        assert_eq!(t.expose_secret(), "supersecretvalue1234");
    }

    #[test]
    fn token_material_trims_ascii_whitespace() {
        let token = normalize_token_material("test token", " \n\tabc.DEF_123+/~==\r\n").unwrap();
        assert_eq!(token, "abc.DEF_123+/~==");
    }

    #[test]
    fn token_material_rejects_malformed_values_without_echoing_secret() {
        for (raw, expected) in [
            (" \n\t ", "must not be empty"),
            ("abc def", "internal whitespace"),
            ("abc\x7fdef", "control characters"),
            ("abcédef", "only ASCII"),
            ("abc:def", "invalid RFC 6750 bearer token characters"),
            ("abc=def", "invalid RFC 6750 bearer token characters"),
            ("====", "at least one bearer token character"),
        ] {
            let err = normalize_token_material("test token", raw)
                .unwrap_err()
                .to_string();
            assert!(err.contains(expected), "err: {err}");
            assert!(!err.contains(raw), "err leaked token material: {err}");
        }
    }

    #[test]
    fn scoped_account_normalizes_registry_url() {
        assert_eq!(
            scoped_account("HTTPS://Registry.Example.com/", DEFAULT_ACCOUNT).unwrap(),
            "registry:https://registry.example.com/v1/:account:default"
        );
        assert_eq!(
            scoped_account("https://registry.example.com/v1", "pilot").unwrap(),
            "registry:https://registry.example.com/v1/:account:pilot"
        );
    }

    #[test]
    fn memory_store_round_trip() {
        let store = MemoryTokenStore::new();
        assert!(store.load(DEFAULT_ACCOUNT).unwrap().is_none());

        store
            .save(DEFAULT_ACCOUNT, &Token::new("hunter2hunter2"))
            .unwrap();
        assert_eq!(
            store
                .load(DEFAULT_ACCOUNT)
                .unwrap()
                .unwrap()
                .expose_secret(),
            "hunter2hunter2"
        );

        store.delete(DEFAULT_ACCOUNT).unwrap();
        assert!(store.load(DEFAULT_ACCOUNT).unwrap().is_none());
    }

    #[test]
    fn memory_store_delete_missing_is_noop() {
        let store = MemoryTokenStore::new();
        store.delete("never-existed").unwrap();
    }

    #[test]
    fn file_store_round_trip() {
        let path = unique_path("file-rt");
        let store = FileTokenStore::new(path.clone());

        assert!(store.load(DEFAULT_ACCOUNT).unwrap().is_none());
        store
            .save(DEFAULT_ACCOUNT, &Token::new("filetokenvalue1"))
            .unwrap();
        assert_eq!(
            store
                .load(DEFAULT_ACCOUNT)
                .unwrap()
                .unwrap()
                .expose_secret(),
            "filetokenvalue1"
        );
        store.delete(DEFAULT_ACCOUNT).unwrap();
        assert!(store.load(DEFAULT_ACCOUNT).unwrap().is_none());

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn default_token_store_uses_config_credentials_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config_dir = unique_path("config-dir");
        unsafe {
            std::env::set_var(crate::config::CONFIG_DIR_ENV, &config_dir);
            std::env::remove_var(TOKEN_FILE_ENV);
            std::env::remove_var(ALLOW_TOKEN_FILE_ENV);
            std::env::remove_var(CREDENTIAL_STORE_ENV);
        }

        let store = token_store();
        assert_eq!(store.kind(), "file");
        let account = scoped_account("https://registry.example.com", DEFAULT_ACCOUNT).unwrap();
        store
            .save(&account, &Token::new("defaultfiletoken1234"))
            .unwrap();
        assert_eq!(
            store.load(&account).unwrap().unwrap().expose_secret(),
            "defaultfiletoken1234"
        );

        let path = default_credentials_file().unwrap();
        assert_eq!(path, config_dir.join("credentials.json"));
        assert!(path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = fs::metadata(&config_dir).unwrap().permissions().mode() & 0o777;
            let file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700);
            assert_eq!(file_mode, 0o600);
        }

        let _ = fs::remove_file(path);
        let _ = fs::remove_dir_all(config_dir);
        unsafe {
            std::env::remove_var(crate::config::CONFIG_DIR_ENV);
            std::env::remove_var(TOKEN_FILE_ENV);
            std::env::remove_var(ALLOW_TOKEN_FILE_ENV);
            std::env::remove_var(CREDENTIAL_STORE_ENV);
        }
    }

    #[cfg(unix)]
    #[test]
    fn default_token_store_secures_existing_config_dir() {
        let _guard = ENV_LOCK.lock().unwrap();
        use std::os::unix::fs::PermissionsExt;

        let config_dir = unique_path("existing-config-dir");
        fs::create_dir_all(&config_dir).unwrap();
        fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o755)).unwrap();
        unsafe {
            std::env::set_var(crate::config::CONFIG_DIR_ENV, &config_dir);
            std::env::remove_var(TOKEN_FILE_ENV);
            std::env::remove_var(ALLOW_TOKEN_FILE_ENV);
            std::env::remove_var(CREDENTIAL_STORE_ENV);
        }

        let store = token_store();
        let account = scoped_account("https://registry.example.com", DEFAULT_ACCOUNT).unwrap();
        store
            .save(&account, &Token::new("existingdirfiletoken1234"))
            .unwrap();

        let mode = fs::metadata(&config_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);

        let _ = fs::remove_dir_all(config_dir);
        unsafe {
            std::env::remove_var(crate::config::CONFIG_DIR_ENV);
            std::env::remove_var(TOKEN_FILE_ENV);
            std::env::remove_var(ALLOW_TOKEN_FILE_ENV);
            std::env::remove_var(CREDENTIAL_STORE_ENV);
        }
    }

    // Bundle every test that touches the process-global `AGENTSTACK_TOKEN`
    // env var into one function. Cargo runs `#[test]`s on a thread pool, so
    // splitting these would race because env vars are shared state.
    #[test]
    fn env_var_resolution_behaves_correctly() {
        let _guard = ENV_LOCK.lock().unwrap();
        let store = MemoryTokenStore::new();
        store
            .save(DEFAULT_ACCOUNT, &Token::new("storedtoken1234"))
            .unwrap();

        // Baseline: nothing in env → falls back to store.
        // SAFETY: env mutation is unsafe in 2024; this test is single-threaded
        // by virtue of being the only one in the suite that touches TOKEN_ENV.
        unsafe {
            std::env::remove_var(TOKEN_ENV);
            std::env::remove_var(TOKEN_PATH_ENV);
        }
        let resolved = resolve_token(&store, DEFAULT_ACCOUNT).unwrap().unwrap();
        assert_eq!(resolved.1, TokenSource::Store);
        assert_eq!(resolved.0.expose_secret(), "storedtoken1234");

        // Empty env var is treated as an invalid token instead of silently
        // falling back to the store.
        unsafe {
            std::env::set_var(TOKEN_ENV, "");
        }
        let err = resolve_token(&store, DEFAULT_ACCOUNT)
            .unwrap_err()
            .to_string();
        assert!(err.contains("AGENTSTACK_TOKEN must not be empty"));

        // A read-only token path is used before the store.
        let token_path = unique_path("path-token");
        fs::write(&token_path, " \npathtoken9876\n").unwrap();
        unsafe {
            std::env::remove_var(TOKEN_ENV);
            std::env::set_var(TOKEN_PATH_ENV, &token_path);
        }
        let resolved = resolve_token(&store, DEFAULT_ACCOUNT).unwrap().unwrap();
        assert_eq!(resolved.0.expose_secret(), "pathtoken9876");
        assert_eq!(resolved.1, TokenSource::Path);

        // Real env var wins over the token path and store after ASCII trim.
        unsafe {
            std::env::set_var(TOKEN_ENV, " \nenvoverridetoken9876\t");
        }
        let resolved = resolve_token(&store, DEFAULT_ACCOUNT).unwrap().unwrap();
        assert_eq!(resolved.0.expose_secret(), "envoverridetoken9876");
        assert_eq!(resolved.1, TokenSource::Env);

        // Malformed env and stored tokens fail without echoing token material.
        unsafe {
            std::env::set_var(TOKEN_ENV, "bad token value");
        }
        let err = resolve_token(&store, DEFAULT_ACCOUNT)
            .unwrap_err()
            .to_string();
        assert!(err.contains("AGENTSTACK_TOKEN must not contain internal whitespace"));
        assert!(!err.contains("bad token value"));

        // No store entry, no env var → None.
        unsafe {
            std::env::remove_var(TOKEN_ENV);
            std::env::remove_var(TOKEN_PATH_ENV);
        }
        let empty_store = MemoryTokenStore::new();
        assert!(
            resolve_token(&empty_store, DEFAULT_ACCOUNT)
                .unwrap()
                .is_none()
        );

        let malformed_store = MemoryTokenStore::new();
        malformed_store
            .save(DEFAULT_ACCOUNT, &Token::new("stored token value"))
            .unwrap();
        let err = resolve_token(&malformed_store, DEFAULT_ACCOUNT)
            .unwrap_err()
            .to_string();
        assert!(err.contains("stored token must not contain internal whitespace"));
        assert!(!err.contains("stored token value"));

        fs::write(&token_path, "bad token value").unwrap();
        unsafe {
            std::env::set_var(TOKEN_PATH_ENV, &token_path);
        }
        let err = resolve_token(&store, DEFAULT_ACCOUNT)
            .unwrap_err()
            .to_string();
        assert!(err.contains("AGENTSTACK_TOKEN_PATH must not contain internal whitespace"));
        assert!(!err.contains("bad token value"));

        let missing = unique_path("missing-path-token");
        unsafe {
            std::env::set_var(TOKEN_PATH_ENV, &missing);
        }
        let err = resolve_token(&store, DEFAULT_ACCOUNT)
            .unwrap_err()
            .to_string();
        assert!(err.contains("failed to inspect AGENTSTACK_TOKEN_PATH file"));
        assert!(err.contains(&missing.display().to_string()));

        unsafe {
            std::env::remove_var(TOKEN_PATH_ENV);
        }
        let _ = fs::remove_file(&token_path);
    }
}
