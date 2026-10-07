//! `[slack]` configuration.
//!
//! Sources, in order (later wins):
//! 1. `[slack]` in `autumn.toml`.
//! 2. `[slack]` in the profile file, for example `autumn-prod.toml`.
//! 3. `.env` values, then the process environment: `AUTUMN_SLACK__<PATH>`,
//!    for example `AUTUMN_SLACK__API__MAX_ATTEMPTS=5`. In an env var, put a
//!    comma between list items.
//!
//! The plugin does not read `[profile.<name>.slack]` in `autumn.toml`. With
//! `[server] strict_config = true`, autumn stops at boot on that section.
//!
//! Secrets are not in the config. The config holds the names of the env vars
//! that hold them.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::SlackError;
use crate::policy::{DEFAULT_TOLERANCE_SECS, MAX_ATTEMPTS, RetryRule};

/// Name of the TOML section.
pub const SECTION: &str = "slack";
/// Prefix for environment overrides.
pub const ENV_PREFIX: &str = "AUTUMN_SLACK__";
/// Slack must get an ack in 3 s, network time included. The ack timeout
/// keeps 300 ms or more for the network.
pub const MAX_ACK_TIMEOUT_MS: u64 = 2_700;

/// Plugin configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct SlackConfig {
    /// Env var with the signing secret.
    pub signing_secret_env: String,
    /// Env vars with old signing secrets. Use during rotation.
    pub previous_signing_secret_envs: Vec<String>,
    /// Env var with the bot token (`xoxb-...`).
    pub bot_token_env: String,
    /// Web API base URL. It ends with `/`. Plain `http` only on a loopback
    /// host (for local tests).
    pub api_base_url: String,
    /// Largest distance between the request timestamp and now, 1 to 3600 s.
    pub timestamp_tolerance_secs: u64,
    /// Time to wait for a command or view handler before an empty ack,
    /// 1 to 2700 ms.
    pub ack_timeout_ms: u64,
    /// Largest request body.
    pub max_body_bytes: usize,
    /// Time to remember an event ID, to drop Slack retries.
    pub dedup_window_secs: u64,
    /// Time to wait for in-flight handlers at shutdown.
    pub drain_timeout_secs: u64,
    /// Reply text when a command handler fails. No error detail goes to Slack.
    pub error_text: String,
    /// Reply text for a command with no handler.
    pub unknown_command_text: String,
    /// Hosts that a `response_url` can use. HTTPS only.
    pub response_url_hosts: Vec<String>,
    /// Web API client settings.
    pub api: ApiConfig,
    /// Health indicator settings.
    pub health: HealthConfig,
}

impl Default for SlackConfig {
    fn default() -> Self {
        Self {
            signing_secret_env: "SLACK_SIGNING_SECRET".to_owned(),
            previous_signing_secret_envs: Vec::new(),
            bot_token_env: "SLACK_BOT_TOKEN".to_owned(),
            api_base_url: "https://slack.com/api/".to_owned(),
            timestamp_tolerance_secs: DEFAULT_TOLERANCE_SECS,
            ack_timeout_ms: 2_500,
            max_body_bytes: 1_048_576,
            dedup_window_secs: 3_600,
            drain_timeout_secs: 10,
            error_text: "Sorry, that did not work. Try again later.".to_owned(),
            unknown_command_text: "This command is not available.".to_owned(),
            response_url_hosts: vec!["hooks.slack.com".to_owned()],
            api: ApiConfig::default(),
            health: HealthConfig::default(),
        }
    }
}

/// Web API client settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct ApiConfig {
    /// Attempts per call, 1 to 10.
    pub max_attempts: u32,
    /// First backoff for 5xx and network errors on read calls.
    pub initial_backoff_ms: u64,
    /// Largest wait before a retry. A longer `Retry-After` stops the call.
    pub max_wait_ms: u64,
    /// Time limit for one HTTP attempt.
    pub timeout_ms: u64,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff_ms: 500,
            max_wait_ms: 30_000,
            timeout_ms: 10_000,
        }
    }
}

impl ApiConfig {
    /// The retry rule for the policy core.
    #[must_use]
    pub const fn rule(&self) -> RetryRule {
        RetryRule {
            max_attempts: self.max_attempts,
            initial_backoff_ms: self.initial_backoff_ms,
            max_wait_ms: self.max_wait_ms,
        }
    }
}

/// Health indicator settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct HealthConfig {
    /// Time to keep an `auth.test` result.
    pub cache_secs: u64,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self { cache_secs: 30 }
    }
}

impl SlackConfig {
    /// Reads `[slack]` from the text of an `autumn.toml` file.
    ///
    /// # Errors
    /// Returns [`SlackError::Config`] for bad TOML, unknown keys, or failed
    /// validation.
    pub fn from_toml_str(autumn_toml: &str) -> Result<Self, SlackError> {
        Self::from_layers(Some(autumn_toml), None, std::iter::empty())
    }

    /// Merges, in order: `[slack]` in `base`, `[slack]` in `profile_file`,
    /// and env vars. Then validates.
    fn from_layers(
        base: Option<&str>,
        profile_file: Option<&str>,
        env: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, SlackError> {
        let parse = |text: &str| -> Result<toml::Table, SlackError> {
            toml::from_str(text).map_err(|e| SlackError::Config(format!("toml: {e}")))
        };
        let mut table = toml::Table::new();
        if let Some(text) = base
            && let Some(toml::Value::Table(section)) = parse(text)?.get(SECTION)
        {
            merge(&mut table, section.clone());
        }
        if let Some(text) = profile_file
            && let Some(toml::Value::Table(section)) = parse(text)?.get(SECTION)
        {
            merge(&mut table, section.clone());
        }
        let schema = toml::Table::try_from(Self::default())
            .map_err(|e| SlackError::Config(e.to_string()))?;
        for (key, value) in env {
            let Some(path) = key.strip_prefix(ENV_PREFIX) else {
                continue;
            };
            let path: Vec<String> = path.split("__").map(str::to_ascii_lowercase).collect();
            // The error names the variable, never the value.
            let typed = typed_env_value(&schema, &path, &value)
                .ok_or_else(|| SlackError::Config(format!("{key}: value is not valid")))?;
            insert_path(&mut table, &path, typed);
        }
        let cfg: Self = toml::Value::Table(table)
            .try_into()
            .map_err(|e| SlackError::Config(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Loads config from files in `manifest_dir` (else the current
    /// directory) and from `env`.
    ///
    /// # Errors
    /// Returns [`SlackError::Config`] for a file that cannot be read or
    /// parsed, or for failed validation.
    pub fn load_from_dir(
        manifest_dir: &Path,
        profile_names: &[String],
        env: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, SlackError> {
        let find = |file: &str| {
            let candidate = manifest_dir.join(file);
            if candidate.exists() {
                candidate
            } else {
                PathBuf::from(file)
            }
        };
        let base = read_optional(&find("autumn.toml"))?;
        let mut profile = None;
        for name in profile_names {
            if let Some(text) = read_optional(&find(&format!("autumn-{name}.toml")))? {
                profile = Some(text);
                break;
            }
        }
        Self::from_layers(base.as_deref(), profile.as_deref(), env)
    }

    /// Loads config like autumn-web does: `$AUTUMN_MANIFEST_DIR` (else `.`),
    /// the app profile, `.env`, and the process environment.
    ///
    /// # Errors
    /// Returns [`SlackError::Config`] when loading or validation fails.
    pub fn load(profile: Option<&str>) -> Result<Self, SlackError> {
        use autumn_web::config::Env as _;
        let os = autumn_web::config::OsEnv;
        let names = profile
            .map(|p| {
                // Same selector order as autumn-web: env vars, then `--profile`.
                let selector = ["AUTUMN_ENV", "AUTUMN_PROFILE"]
                    .iter()
                    .find_map(|k| os.var(k).ok().filter(|v| !v.trim().is_empty()))
                    .or_else(profile_flag)
                    .map_or_else(|| p.to_owned(), |v| v.trim().to_owned());
                autumn_web::config::profile_override_file_lookup_names(p, &selector)
            })
            .unwrap_or_default();
        let dir = os
            .var("AUTUMN_MANIFEST_DIR")
            .map_or_else(|_| PathBuf::from("."), PathBuf::from);
        Self::load_from_dir(&dir, &names, process_env()?)
    }

    /// Checks the values.
    ///
    /// # Errors
    /// Returns [`SlackError::Config`] with the first problem found.
    pub fn validate(&self) -> Result<(), SlackError> {
        let bad = |m: String| Err(SlackError::Config(m));
        for (name, value) in [
            ("signing_secret_env", &self.signing_secret_env),
            ("bot_token_env", &self.bot_token_env),
            ("error_text", &self.error_text),
            ("unknown_command_text", &self.unknown_command_text),
        ] {
            if value.trim().is_empty() {
                return bad(format!("{name} must not be empty"));
            }
        }
        if self
            .previous_signing_secret_envs
            .iter()
            .any(|v| v.trim().is_empty())
        {
            return bad("previous_signing_secret_envs must not hold an empty name".to_owned());
        }
        if !valid_base_url(&self.api_base_url) {
            return bad(format!(
                "api_base_url must be an https URL that ends with / (http only on loopback): {}",
                self.api_base_url
            ));
        }
        if !(1..=3_600).contains(&self.timestamp_tolerance_secs) {
            return bad(format!(
                "timestamp_tolerance_secs must be 1 to 3600, not {}",
                self.timestamp_tolerance_secs
            ));
        }
        if !(1..=MAX_ACK_TIMEOUT_MS).contains(&self.ack_timeout_ms) {
            return bad(format!(
                "ack_timeout_ms must be 1 to {MAX_ACK_TIMEOUT_MS}, not {}",
                self.ack_timeout_ms
            ));
        }
        if self.max_body_bytes == 0 {
            return bad("max_body_bytes must be 1 or more".to_owned());
        }
        if self.response_url_hosts.is_empty()
            || self
                .response_url_hosts
                .iter()
                .any(|h| h.trim().is_empty() || h.contains('/'))
        {
            return bad("response_url_hosts must list one or more host names".to_owned());
        }
        if !(1..=MAX_ATTEMPTS).contains(&self.api.max_attempts) {
            return bad(format!(
                "api.max_attempts must be 1 to {MAX_ATTEMPTS}, not {}",
                self.api.max_attempts
            ));
        }
        if self.api.timeout_ms == 0 {
            return bad("api.timeout_ms must be 1 or more".to_owned());
        }
        Ok(())
    }
}

/// Reads one variable from the process environment, else from `.env`.
pub(crate) fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().or_else(|| {
        autumn_web::dotenv::resolve_process_dotenv()
            .ok()?
            .into_iter()
            .find_map(|(k, v)| (k == name).then_some(v))
    })
}

/// Returns `.env` values, then the process environment (later wins).
///
/// It skips a variable whose name or value is not UTF-8.
fn process_env() -> Result<Vec<(String, String)>, SlackError> {
    let mut vars = autumn_web::dotenv::resolve_process_dotenv()
        .map_err(|e| SlackError::Config(format!(".env: {e}")))?;
    vars.extend(
        std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))),
    );
    Ok(vars)
}

/// The value of `--profile <name>` or `--profile=<name>` in the process args.
fn profile_flag() -> Option<String> {
    let args: Vec<String> = std::env::args_os()
        .filter_map(|a| a.into_string().ok())
        .collect();
    args.iter()
        .enumerate()
        .find_map(|(i, a)| {
            a.strip_prefix("--profile=").map(str::to_owned).or_else(|| {
                (a == "--profile")
                    .then(|| args.get(i + 1).cloned())
                    .flatten()
            })
        })
        // Like autumn-web: an empty value selects nothing.
        .filter(|p| !p.trim().is_empty())
}

/// An `https` URL that ends with `/`, or `http` on a loopback host.
fn valid_base_url(url: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else {
        return false;
    };
    let loopback = match u.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    url.ends_with('/') && (u.scheme() == "https" || (u.scheme() == "http" && loopback))
}

fn read_optional(path: &Path) -> Result<Option<String>, SlackError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(SlackError::Config(format!("{}: {e}", path.display()))),
    }
}

/// Deep merge: tables merge by key; other values replace.
fn merge(into: &mut toml::Table, from: toml::Table) {
    for (key, value) in from {
        match (into.get_mut(&key), value) {
            (Some(toml::Value::Table(dst)), toml::Value::Table(src)) => merge(dst, src),
            (_, value) => {
                into.insert(key, value);
            }
        }
    }
}

fn insert_path(table: &mut toml::Table, path: &[String], value: toml::Value) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut cur = table;
    for key in parents {
        let entry = cur
            .entry(key.clone())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if !entry.is_table() {
            *entry = toml::Value::Table(toml::Table::new());
        }
        let toml::Value::Table(next) = entry else {
            return;
        };
        cur = next;
    }
    cur.insert(last.clone(), value);
}

/// Types an env value like the default value at the same path. A list is a
/// comma-separated string.
fn typed_env_value(schema: &toml::Table, path: &[String], raw: &str) -> Option<toml::Value> {
    let mut node: Option<&toml::Value> = None;
    let mut table = Some(schema);
    for key in path {
        node = table.and_then(|t| t.get(key));
        table = node.and_then(toml::Value::as_table);
    }
    match node {
        Some(toml::Value::Integer(_)) => raw.trim().parse().ok().map(toml::Value::Integer),
        Some(toml::Value::Boolean(_)) => raw.trim().parse().ok().map(toml::Value::Boolean),
        Some(toml::Value::Array(_)) => Some(toml::Value::Array(
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| toml::Value::String(s.to_owned()))
                .collect(),
        )),
        _ => Some(toml::Value::String(raw.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn defaults_are_valid() {
        SlackConfig::default().validate().unwrap();
        assert_eq!(
            SlackConfig::from_toml_str("").unwrap(),
            SlackConfig::default()
        );
    }

    #[test]
    fn reads_section_and_rejects_unknown_keys() {
        let c = SlackConfig::from_toml_str(
            "[slack]\nack_timeout_ms = 900\n[slack.api]\nmax_attempts = 1\n",
        )
        .unwrap();
        assert_eq!(c.ack_timeout_ms, 900);
        assert_eq!(c.api.max_attempts, 1);
        assert!(SlackConfig::from_toml_str("[slack]\nack_timeout = 9\n").is_err());
        assert!(SlackConfig::from_toml_str("[slack\n").is_err());
        // Other sections are not ours.
        SlackConfig::from_toml_str("[server]\nport = 1\n").unwrap();
    }

    #[test]
    fn env_overlay_types_values() {
        let c = SlackConfig::from_layers(
            None,
            None,
            env(&[
                ("AUTUMN_SLACK__ACK_TIMEOUT_MS", "100"),
                ("AUTUMN_SLACK__BOT_TOKEN_ENV", "MY_TOKEN"),
                ("AUTUMN_SLACK__PREVIOUS_SIGNING_SECRET_ENVS", "A,B"),
                ("AUTUMN_SLACK__HEALTH__CACHE_SECS", "5"),
            ]),
        )
        .unwrap();
        assert_eq!(c.ack_timeout_ms, 100);
        assert_eq!(c.bot_token_env, "MY_TOKEN");
        assert_eq!(c.previous_signing_secret_envs, ["A", "B"]);
        assert_eq!(c.health.cache_secs, 5);
    }

    #[test]
    fn validation_ranges() {
        let check = |f: fn(&mut SlackConfig)| {
            let mut c = SlackConfig::default();
            f(&mut c);
            c.validate().unwrap_err().to_string()
        };
        assert!(check(|c| c.ack_timeout_ms = 0).contains("ack_timeout_ms"));
        assert!(check(|c| c.ack_timeout_ms = 3_000).contains("ack_timeout_ms"));
        assert!(check(|c| c.timestamp_tolerance_secs = 0).contains("timestamp_tolerance_secs"));
        assert!(check(|c| c.timestamp_tolerance_secs = 3_601).contains("timestamp_tolerance_secs"));
        assert!(check(|c| c.api.max_attempts = 0).contains("max_attempts"));
        assert!(check(|c| c.api.max_attempts = MAX_ATTEMPTS + 1).contains("max_attempts"));
        assert!(check(|c| c.api.timeout_ms = 0).contains("timeout_ms"));
        assert!(check(|c| c.api_base_url = "ftp://x/".into()).contains("api_base_url"));
        assert!(
            check(|c| c.api_base_url = "https://slack.com/api".into()).contains("api_base_url")
        );
        assert!(check(|c| c.response_url_hosts.clear()).contains("response_url_hosts"));
        assert!(
            check(|c| c.response_url_hosts = vec![String::new()]).contains("response_url_hosts")
        );
        assert!(check(|c| c.max_body_bytes = 0).contains("max_body_bytes"));
        assert!(check(|c| c.signing_secret_env.clear()).contains("signing_secret_env"));
        assert!(check(|c| c.bot_token_env.clear()).contains("bot_token_env"));
        assert!(check(|c| c.error_text.clear()).contains("error_text"));
    }

    #[test]
    fn inline_profile_section_is_not_read() {
        // autumn strict config rejects `[profile.x.slack]`, so the plugin ignores it.
        let base = "[slack]\nack_timeout_ms = 1\n[profile.prod.slack]\nack_timeout_ms = 3\n";
        let c = SlackConfig::from_toml_str(base).unwrap();
        assert_eq!(c.ack_timeout_ms, 1);
    }

    #[test]
    fn base_url_rules() {
        for ok in [
            "https://slack.com/api/",
            "http://127.0.0.1:9/api/",
            "http://localhost/api/",
            "http://[::1]:8/x/",
        ] {
            assert!(valid_base_url(ok), "{ok}");
        }
        for bad in [
            "http://slack.com/api/",
            "http://10.0.0.1/api/",
            "https://slack.com/api",
            "ftp://x/",
            "nope",
        ] {
            assert!(!valid_base_url(bad), "{bad}");
        }
    }
}
