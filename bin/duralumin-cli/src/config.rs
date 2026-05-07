use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{de, Deserialize, Deserializer};

use duralumin_core::Action;
use duralumin_rules::config::{FeedConfig, RuleConfig, validate_rules};

// ---- Serde helpers ---------------------------------------------------------

fn de_duration<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
    humantime::parse_duration(&String::deserialize(d)?).map_err(de::Error::custom)
}

// ---- Defaults --------------------------------------------------------------

fn default_concurrent_downloads() -> u32 { 2 }
fn default_attempt_timeout() -> Duration { Duration::from_secs(20 * 60) }
fn default_max_retries() -> u8 { 3 }
fn default_backoff_base() -> Duration { Duration::from_secs(30) }
fn default_user_agent() -> String {
    format!("duralumin/{} (+https://github.com/yourname/duralumin)", env!("CARGO_PKG_VERSION"))
}
fn default_log_format() -> String { "pretty".into() }
fn default_log_level() -> String { "info".into() }
fn default_action_on_no_match() -> Action { Action::Skip }

// ---- Error type ------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config file not found — use --config <PATH> or set $DURALUMIN_CONFIG")]
    NotFound,
    #[error("cannot read {0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),
    #[error("TOML parse error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("validation failed:\n  {}", .0.join("\n  "))]
    Validation(Vec<String>),
}

// ---- Config structs --------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct Config {
    pub storage: StorageConfig,
    #[serde(default)]
    pub downloader: DownloaderConfig,
    #[serde(default)]
    pub defaults: DefaultsConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub feeds: Vec<FeedConfig>,
    #[serde(default)]
    pub global_rules: Vec<RuleConfig>,
}

#[derive(Debug, Deserialize)]
pub struct StorageConfig {
    pub library_path: PathBuf,
    pub state_db: PathBuf,
}

#[derive(Debug, Deserialize)]
pub struct DownloaderConfig {
    #[serde(default = "default_concurrent_downloads")]
    pub concurrent_downloads: u32,
    #[serde(default = "default_attempt_timeout", deserialize_with = "de_duration")]
    pub attempt_timeout: Duration,
    #[serde(default = "default_max_retries")]
    pub max_retries: u8,
    #[serde(default = "default_backoff_base", deserialize_with = "de_duration")]
    pub backoff_base: Duration,
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
}

impl Default for DownloaderConfig {
    fn default() -> Self {
        Self {
            concurrent_downloads: default_concurrent_downloads(),
            attempt_timeout: default_attempt_timeout(),
            max_retries: default_max_retries(),
            backoff_base: default_backoff_base(),
            user_agent: default_user_agent(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct DefaultsConfig {
    #[serde(default = "default_action_on_no_match")]
    pub action_on_no_match: Action,
}

impl Default for DefaultsConfig {
    fn default() -> Self {
        Self { action_on_no_match: default_action_on_no_match() }
    }
}

#[derive(Debug, Deserialize)]
pub struct LoggingConfig {
    /// `"pretty"` (default, colored) or `"json"` (one object per line).
    #[serde(default = "default_log_format")]
    pub format: String,
    #[serde(default = "default_log_level")]
    pub level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self { format: default_log_format(), level: default_log_level() }
    }
}

// ---- Load + validate -------------------------------------------------------

pub fn load(override_path: Option<&Path>) -> Result<Config, ConfigError> {
    let path = resolve_path(override_path)?;
    let text = std::fs::read_to_string(&path)
        .map_err(|e| ConfigError::Io(path.clone(), e))?;
    let cfg: Config = toml::from_str(&text)?;
    validate(&cfg)?;
    Ok(cfg)
}

fn validate(cfg: &Config) -> Result<(), ConfigError> {
    let mut errors = Vec::new();

    // Slug uniqueness
    let mut seen = std::collections::HashSet::new();
    for feed in &cfg.feeds {
        if !seen.insert(feed.slug.as_str()) {
            errors.push(format!("duplicate feed slug: {:?}", feed.slug));
        }
    }

    // Per-feed rule validation (regex compile, etc.)
    for feed in &cfg.feeds {
        for msg in validate_rules(&feed.rules) {
            errors.push(format!("[feed {}] {msg}", feed.slug));
        }
    }

    // Global rule validation
    for msg in validate_rules(&cfg.global_rules) {
        errors.push(format!("[global] {msg}"));
    }

    if errors.is_empty() { Ok(()) } else { Err(ConfigError::Validation(errors)) }
}

/// Resolve config file path in the order specified by spec §7.
fn resolve_path(override_path: Option<&Path>) -> Result<PathBuf, ConfigError> {
    if let Some(p) = override_path {
        return Ok(p.to_path_buf());
    }

    if let Ok(val) = std::env::var("DURALUMIN_CONFIG") {
        return Ok(PathBuf::from(val));
    }

    // XDG / platform default
    let config_home = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|_| {
            // On Windows use APPDATA, elsewhere fall back to ~/.config
            #[cfg(target_os = "windows")]
            return std::env::var("APPDATA").map(PathBuf::from);
            #[cfg(not(target_os = "windows"))]
            std::env::var("HOME")
                .map(|h| PathBuf::from(h).join(".config"))
                .map_err(|e| e)
        })
        .unwrap_or_else(|_| PathBuf::from(".config"));

    let xdg = config_home.join("duralumin/config.toml");
    if xdg.exists() {
        return Ok(xdg);
    }

    // System-wide fallback (Linux only)
    #[cfg(target_os = "linux")]
    {
        let etc = PathBuf::from("/etc/duralumin/config.toml");
        if etc.exists() {
            return Ok(etc);
        }
    }

    Err(ConfigError::NotFound)
}
