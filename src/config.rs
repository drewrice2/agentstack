//! AgentStack configuration and cache paths, plus the [`ConfigStore`] used to
//! read and write `config.toml`.
//!
//! The on-disk format is intentionally small:
//!
//! ```toml
//! [registry]
//! url = "https://registry.example.com"
//!
//! [targets]
//! claude-code = "/custom/path"
//! codex       = "/custom/path"
//! local       = "/custom/path"
//! ```
//!
//! Auth tokens are **never** stored in `config.toml` — they live in the
//! credential store (see [`crate::credentials`]). New top-level keys can be
//! added freely because every field is `#[serde(default)]`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};

const AGENTSTACK_DIR_NAME: &str = ".agentstack";
const CACHE_DIR_NAME: &str = "cache";

/// Env var that overrides the default cache location. Useful for tests and
/// for users who keep their tools out of `~/.agentstack/cache`.
pub const CACHE_DIR_ENV: &str = "AGENTSTACK_CACHE_DIR";
/// Env var that overrides the default config location.
pub const CONFIG_DIR_ENV: &str = "AGENTSTACK_CONFIG_DIR";
/// Env var that overrides the persisted registry URL for headless use.
pub const REGISTRY_URL_ENV: &str = "AGENTSTACK_REGISTRY_URL";
/// Built-in registry URL used when no env override or persisted config exists.
pub const DEFAULT_REGISTRY_URL: &str = "https://registry.agentstack.gg";

/// Filename written under [`config_dir()`].
pub const CONFIG_FILE_NAME: &str = "config.toml";

fn home_dir() -> Result<PathBuf> {
    Ok(BaseDirs::new()
        .context("could not determine a home directory for the current user")?
        .home_dir()
        .to_path_buf())
}

pub fn config_dir() -> Result<PathBuf> {
    if let Some(p) = env_path(CONFIG_DIR_ENV) {
        return Ok(p);
    }
    Ok(home_dir()?.join(AGENTSTACK_DIR_NAME))
}

pub fn cache_dir() -> Result<PathBuf> {
    if let Some(p) = env_path(CACHE_DIR_ENV) {
        return Ok(p);
    }
    Ok(home_dir()?.join(AGENTSTACK_DIR_NAME).join(CACHE_DIR_NAME))
}

pub fn config_file() -> Result<PathBuf> {
    Ok(config_dir()?.join(CONFIG_FILE_NAME))
}

fn env_path(var: &str) -> Option<PathBuf> {
    let raw = std::env::var_os(var)?;
    if raw.is_empty() {
        return None;
    }
    Some(PathBuf::from(raw))
}

fn env_string(var: &str) -> Option<String> {
    let raw = std::env::var_os(var)?;
    if raw.is_empty() {
        return None;
    }
    Some(raw.to_string_lossy().into_owned())
}

/// Non-secret registry settings. Tokens live in the credential store, never
/// here.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegistryConfig {
    /// Base URL for the hosted registry (e.g. `https://registry.example.com`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl RegistryConfig {
    pub fn is_empty(&self) -> bool {
        self.url.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryUrlSource {
    Env,
    Config,
    Default,
}

impl RegistryUrlSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Env => REGISTRY_URL_ENV,
            Self::Config => "config",
            Self::Default => "default",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRegistryUrl {
    pub url: String,
    pub source: RegistryUrlSource,
}

/// On-disk shape of `config.toml`. New top-level keys can be added later
/// without breaking older configs because every field is `#[serde(default)]`.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentStackConfig {
    /// Map of install-target name → absolute filesystem path.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub targets: BTreeMap<String, PathBuf>,
    /// Hosted registry settings (URL etc.).
    #[serde(default, skip_serializing_if = "RegistryConfig::is_empty")]
    pub registry: RegistryConfig,
}

impl AgentStackConfig {
    /// True when nothing is set — useful so `config show` can hint at next steps.
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty() && self.registry.is_empty()
    }
}

/// File-backed handle to [`AgentStackConfig`].
///
/// `ConfigStore::load` is fail-soft: a missing file produces an empty store
/// pointed at the path we *would* write. Malformed TOML is a hard error so
/// users notice typos instead of silently losing settings.
#[derive(Debug, Clone)]
pub struct ConfigStore {
    path: PathBuf,
    config: AgentStackConfig,
}

impl ConfigStore {
    /// Load from `~/.agentstack/config.toml` (or `AGENTSTACK_CONFIG_DIR`).
    pub fn load() -> Result<Self> {
        Self::load_from(config_file()?)
    }

    /// Load from an explicit path. Intended for tests and `--config` flows.
    pub fn load_from(path: PathBuf) -> Result<Self> {
        let config = if path.exists() {
            let text = fs::read_to_string(&path)
                .with_context(|| format!("failed to read `{}`", path.display()))?;
            toml::from_str::<AgentStackConfig>(&text)
                .with_context(|| format!("failed to parse `{}`", path.display()))?
        } else {
            AgentStackConfig::default()
        };
        validate_target_paths(&config, &path)?;
        Ok(Self { path, config })
    }

    /// Path to the backing `config.toml` (whether or not it exists yet).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Borrow the parsed configuration.
    pub fn config(&self) -> &AgentStackConfig {
        &self.config
    }

    /// Override path for `target_name`, if any.
    pub fn target_override(&self, target_name: &str) -> Option<&Path> {
        self.config.targets.get(target_name).map(PathBuf::as_path)
    }

    /// Set or replace the override for `target_name`.
    pub fn set_target(&mut self, target_name: String, path: PathBuf) {
        self.config.targets.insert(target_name, path);
    }

    /// Remove the override for `target_name`. Returns the previous value.
    pub fn unset_target(&mut self, target_name: &str) -> Option<PathBuf> {
        self.config.targets.remove(target_name)
    }

    /// Currently configured registry URL, if any.
    pub fn registry_url(&self) -> Option<&str> {
        self.config.registry.url.as_deref()
    }

    /// Effective registry URL, preferring the headless env override and
    /// falling back to [`DEFAULT_REGISTRY_URL`].
    pub fn resolved_registry_url(&self) -> ResolvedRegistryUrl {
        if let Some(url) = env_string(REGISTRY_URL_ENV) {
            return ResolvedRegistryUrl {
                url,
                source: RegistryUrlSource::Env,
            };
        }
        if let Some(url) = self.registry_url() {
            return ResolvedRegistryUrl {
                url: url.to_string(),
                source: RegistryUrlSource::Config,
            };
        }
        ResolvedRegistryUrl {
            url: DEFAULT_REGISTRY_URL.to_string(),
            source: RegistryUrlSource::Default,
        }
    }

    /// Set (or replace) the registry URL.
    pub fn set_registry_url(&mut self, url: String) {
        self.config.registry.url = Some(url);
    }

    /// Persist the current configuration. Creates parent directories as needed.
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create `{}`", parent.display()))?;
        }
        let text = toml::to_string_pretty(&self.config)
            .context("failed to serialize agentstack config")?;
        crate::fs_atomic::write_string(&self.path, &text)
            .with_context(|| format!("failed to write `{}`", self.path.display()))?;
        Ok(())
    }
}

fn validate_target_paths(config: &AgentStackConfig, config_path: &Path) -> Result<()> {
    for (target, path) in &config.targets {
        if !path.is_absolute() {
            anyhow::bail!(
                "target override path for `{target}` must be absolute in `{}` (got `{}`)",
                config_path.display(),
                path.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};

    fn registry_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn tmp_path(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agentstack-config-{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir.join("config.toml")
    }

    #[test]
    fn missing_file_loads_empty_store() {
        let path = tmp_path("missing");
        let store = ConfigStore::load_from(path.clone()).unwrap();
        assert_eq!(store.path(), path);
        assert!(store.config().is_empty());
    }

    #[test]
    fn round_trip_preserves_targets() {
        let path = tmp_path("roundtrip");
        let mut store = ConfigStore::load_from(path.clone()).unwrap();
        store.set_target("claude-code".into(), PathBuf::from("/custom/claude"));
        store.set_target("codex".into(), PathBuf::from("/custom/codex"));
        store.save().unwrap();

        let reloaded = ConfigStore::load_from(path).unwrap();
        assert_eq!(
            reloaded.target_override("claude-code"),
            Some(Path::new("/custom/claude"))
        );
        assert_eq!(
            reloaded.target_override("codex"),
            Some(Path::new("/custom/codex"))
        );
        assert_eq!(reloaded.target_override("local"), None);
    }

    #[test]
    fn load_rejects_hand_edited_relative_target_path() {
        let path = tmp_path("relative-target");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "[targets]\ncodex = \"relative/codex-skills\"\n").unwrap();

        let err = ConfigStore::load_from(path).unwrap_err();
        assert!(err.to_string().contains("must be absolute"));
        assert!(err.to_string().contains("codex"));
        assert!(err.to_string().contains("relative/codex-skills"));
    }

    #[test]
    fn unset_target_removes_entry() {
        let path = tmp_path("unset");
        let mut store = ConfigStore::load_from(path).unwrap();
        store.set_target("local".into(), PathBuf::from("/x"));
        assert!(store.unset_target("local").is_some());
        assert!(store.unset_target("local").is_none());
        assert!(store.config().is_empty());
    }

    #[test]
    fn registry_url_round_trips() {
        let path = tmp_path("registry");
        let mut store = ConfigStore::load_from(path.clone()).unwrap();
        assert_eq!(store.registry_url(), None);

        store.set_registry_url("https://registry.example.com".into());
        store.save().unwrap();

        let reloaded = ConfigStore::load_from(path).unwrap();
        assert_eq!(
            reloaded.registry_url(),
            Some("https://registry.example.com")
        );
    }

    #[test]
    fn resolved_registry_url_uses_default_when_unconfigured() {
        let _guard = registry_env_lock().lock().unwrap();
        unsafe {
            std::env::remove_var(REGISTRY_URL_ENV);
        }

        let store = ConfigStore::load_from(tmp_path("registry-default")).unwrap();
        let resolved = store.resolved_registry_url();
        assert_eq!(resolved.url, DEFAULT_REGISTRY_URL);
        assert_eq!(resolved.source, RegistryUrlSource::Default);
    }

    #[test]
    fn resolved_registry_url_env_wins_over_default() {
        let _guard = registry_env_lock().lock().unwrap();
        let store = ConfigStore::load_from(tmp_path("registry-env-default")).unwrap();

        unsafe {
            std::env::set_var(REGISTRY_URL_ENV, "https://env.registry.example.com");
        }
        let resolved = store.resolved_registry_url();
        assert_eq!(resolved.url, "https://env.registry.example.com");
        assert_eq!(resolved.source, RegistryUrlSource::Env);

        unsafe {
            std::env::remove_var(REGISTRY_URL_ENV);
        }
    }

    #[test]
    fn resolved_registry_url_config_wins_over_default_but_loses_to_env() {
        let _guard = registry_env_lock().lock().unwrap();
        let path = tmp_path("registry-precedence");
        let mut store = ConfigStore::load_from(path).unwrap();
        store.set_registry_url("https://registry.example.com".into());

        unsafe {
            std::env::remove_var(REGISTRY_URL_ENV);
        }
        let resolved = store.resolved_registry_url();
        assert_eq!(resolved.url, "https://registry.example.com");
        assert_eq!(resolved.source, RegistryUrlSource::Config);

        unsafe {
            std::env::set_var(REGISTRY_URL_ENV, "https://env.registry.example.com");
        }
        let resolved = store.resolved_registry_url();
        assert_eq!(resolved.url, "https://env.registry.example.com");
        assert_eq!(resolved.source, RegistryUrlSource::Env);

        unsafe {
            std::env::set_var(REGISTRY_URL_ENV, "");
        }
        let resolved = store.resolved_registry_url();
        assert_eq!(resolved.url, "https://registry.example.com");
        assert_eq!(resolved.source, RegistryUrlSource::Config);

        unsafe {
            std::env::remove_var(REGISTRY_URL_ENV);
        }
    }

    #[test]
    fn registry_section_does_not_leak_token_field() {
        // Defense-in-depth: if anyone ever adds a `token` field to RegistryConfig,
        // this test fails because the serialized form should never contain it.
        let mut store = ConfigStore::load_from(tmp_path("no-token")).unwrap();
        store.set_registry_url("https://x".into());
        let text = toml::to_string_pretty(store.config()).unwrap();
        assert!(
            !text.to_lowercase().contains("token"),
            "serialized config must not contain a token field, got:\n{text}"
        );
    }
}
