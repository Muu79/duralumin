use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_CONFIG: &str = include_str!("default_config.toml");

use serde::{Deserialize, Deserializer, de};

use duralumin_core::Action;
use duralumin_rules::config::{FeedConfig, RuleConfig, validate_rules};
pub use duralumin_server::ServerConfig;

// ---- Serde helpers ---------------------------------------------------------

fn de_duration<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
    humantime::parse_duration(&String::deserialize(d)?).map_err(de::Error::custom)
}

// ---- Defaults --------------------------------------------------------------

fn default_concurrent_downloads() -> u32 {
    2
}
fn default_attempt_timeout() -> Duration {
    Duration::from_secs(20 * 60)
}
fn default_max_retries() -> u8 {
    3
}
fn default_backoff_base() -> Duration {
    Duration::from_secs(30)
}
fn default_user_agent() -> String {
    format!(
        "duralumin/{} (+https://github.com/Muu79/duralumin)",
        env!("CARGO_PKG_VERSION")
    )
}
fn default_accept_invalid_certs() -> bool {
    false
}
fn default_max_bytes_per_sec() -> u64 {
    0
}
fn default_log_level() -> String {
    "info".into()
}
fn default_log_format() -> String {
    "pretty".into()
}
fn default_action_on_no_match() -> Action {
    Action::Skip
}

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
    /// A default config was just written; caller should print the path and exit 0.
    #[error("generated default config at {0}")]
    Generated(PathBuf),
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
    pub server: Option<ServerConfig>,
}

#[derive(Debug, Deserialize)]
pub struct StorageConfig {
    /// Base directory. `library_path` and `state_db` default relative to this.
    pub dir: PathBuf,
    /// Override for the podcast library root. Defaults to `{dir}/podcasts`.
    pub library_path: Option<PathBuf>,
    /// Override for the SQLite database file. Defaults to `{dir}/db/duralumin.db`.
    pub state_db: Option<PathBuf>,
}

impl StorageConfig {
    pub fn library(&self) -> PathBuf {
        self.library_path
            .clone()
            .unwrap_or_else(|| self.dir.join("podcasts"))
    }
    pub fn db(&self) -> PathBuf {
        self.state_db
            .clone()
            .unwrap_or_else(|| self.dir.join("db").join("duralumin.db"))
    }
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
    #[serde(default = "default_accept_invalid_certs")]
    pub accept_invalid_certs: bool,
    /// Per-download bandwidth cap in bytes per second. `0` = uncapped (default).
    #[serde(default = "default_max_bytes_per_sec")]
    pub max_bytes_per_sec: u64,
}

impl Default for DownloaderConfig {
    fn default() -> Self {
        Self {
            concurrent_downloads: default_concurrent_downloads(),
            attempt_timeout: default_attempt_timeout(),
            max_retries: default_max_retries(),
            backoff_base: default_backoff_base(),
            user_agent: default_user_agent(),
            accept_invalid_certs: default_accept_invalid_certs(),
            max_bytes_per_sec: default_max_bytes_per_sec(),
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
        Self {
            action_on_no_match: default_action_on_no_match(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct LoggingConfig {
    /// Standard log levels: error, warn, info, debug, trace.
    #[serde(default = "default_log_level")]
    pub level: String,
    /// Output format: `"pretty"` (default, human-readable) or `"json"` (one object per line).
    #[serde(default = "default_log_format")]
    pub format: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            format: default_log_format(),
        }
    }
}

// ---- Load + validate -------------------------------------------------------

pub fn load(override_path: Option<&Path>) -> Result<(Config, PathBuf), ConfigError> {
    let path = match resolve_path(override_path) {
        Ok(p) => p,
        Err(ConfigError::NotFound) => {
            let dest = bootstrap_path();
            write_default_config(&dest)?;
            // Return a special variant so main can print the right message and exit.
            return Err(ConfigError::Generated(dest));
        }
        Err(e) => return Err(e),
    };
    let text = std::fs::read_to_string(&path).map_err(|e| ConfigError::Io(path.clone(), e))?;
    let cfg: Config = toml::from_str(&text)?;
    validate(&cfg)?;
    Ok((cfg, path))
}

fn validate(cfg: &Config) -> Result<(), ConfigError> {
    let mut errors = Vec::new();

    // All slugs and aliases must be unique across the entire config.
    // key = identifier, value = human-readable description for error context.
    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for feed in &cfg.feeds {
        if feed.slug.is_empty() {
            errors.push("feed has an empty slug".into());
            continue;
        }

        let slug_desc = format!("slug of feed {:?}", feed.slug);
        if let Some(prev) = seen.insert(feed.slug.clone(), slug_desc) {
            errors.push(format!("feed slug {:?} conflicts with {prev}", feed.slug));
        }

        for alias in &feed.aliases {
            if alias.is_empty() {
                errors.push(format!("[feed {}] alias must not be empty", feed.slug));
                continue;
            }
            let alias_desc = format!("alias {:?} of feed {:?}", alias, feed.slug);
            if let Some(prev) = seen.insert(alias.clone(), alias_desc) {
                errors.push(format!(
                    "[feed {}] alias {:?} conflicts with {prev}",
                    feed.slug, alias
                ));
            }
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

    if errors.is_empty() {
        Ok(())
    } else {
        Err(ConfigError::Validation(errors))
    }
}

/// Determine where to write a new default config on first run.
///
/// Priority (first writable wins):
///   Linux/macOS : $XDG_CONFIG_HOME/duralumin/config.toml
///              or $HOME/.config/duralumin/config.toml
///   Windows     : %APPDATA%\duralumin\config.toml
///   Fallback    : ./duralumin.toml  (current working directory)
fn bootstrap_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    let base: Option<PathBuf> = std::env::var("APPDATA").ok().map(PathBuf::from);

    #[cfg(not(target_os = "windows"))]
    let base: Option<PathBuf> = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        });

    if let Some(dir) = base {
        return dir.join("duralumin").join("config.toml");
    }

    PathBuf::from("duralumin.toml")
}

fn write_default_config(dest: &Path) -> Result<(), ConfigError> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ConfigError::Io(parent.to_path_buf(), e))?;
    }
    std::fs::write(dest, DEFAULT_CONFIG).map_err(|e| ConfigError::Io(dest.to_path_buf(), e))
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
            std::env::var("HOME").map(|h| PathBuf::from(h).join(".config"))
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

    // Local fallback — current working directory (also where bootstrap writes on failure)
    let local = PathBuf::from("duralumin.toml");
    if local.exists() {
        return Ok(local);
    }

    Err(ConfigError::NotFound)
}
