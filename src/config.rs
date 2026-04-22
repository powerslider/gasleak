use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// On-disk config shape. All fields optional so an empty file is valid.
/// Unknown fields are ignored to keep the file forward-compatible.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct FileConfig {
    pub inactive: InactiveConfig,
    pub underutilized: UnderutilizedConfig,
    pub long_lived: LongLivedConfig,
    pub warn: WarnConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct InactiveConfig {
    /// Days since last activity below which the rule does not fire.
    pub low_days: Option<i64>,
    /// Days at or above which the verdict severity is Medium.
    pub medium_days: Option<i64>,
    /// Days at or above which the verdict severity is High.
    pub high_days: Option<i64>,
    /// CloudWatch sample floor. Below this the rule does not fire.
    pub min_samples: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct LongLivedConfig {
    /// Total instance age at or above which the `long_lived` verdict fires.
    pub age_days: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct UnderutilizedConfig {
    /// p95 CPU % below which the `underutilized` warning fires.
    pub p95_threshold_pct: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct WarnConfig {
    /// Lead-time before `ExpiresAt` for the `expiring_soon` verdict.
    pub window_hours: Option<i64>,
}

/// Resolved config source, plus whether it was explicitly requested by the
/// user. Explicit paths (CLI `--config` or `$GASLEAK_CONFIG`) error on missing
/// file. The default HOME path silently falls back to `FileConfig::default()`.
struct ConfigPath {
    path: PathBuf,
    is_explicit: bool,
}

/// Resolve which config file to load.
///
/// Precedence, highest first:
/// 1. `--config <PATH>` CLI flag. Explicit, errors if missing.
/// 2. `$GASLEAK_CONFIG` env var. Explicit, errors if missing.
/// 3. `$HOME/.config/gasleak/gasleak.toml`. Default, silent if missing.
fn resolve(cli_override: Option<&Path>) -> Option<ConfigPath> {
    if let Some(p) = cli_override {
        return Some(ConfigPath {
            path: p.to_path_buf(),
            is_explicit: true,
        });
    }
    if let Ok(explicit) = std::env::var("GASLEAK_CONFIG")
        && !explicit.is_empty()
    {
        return Some(ConfigPath {
            path: PathBuf::from(explicit),
            is_explicit: true,
        });
    }
    let home = std::env::var("HOME").ok()?;
    if home.is_empty() {
        return None;
    }
    Some(ConfigPath {
        path: PathBuf::from(home)
            .join(".config")
            .join("gasleak")
            .join("gasleak.toml"),
        is_explicit: false,
    })
}

/// Load config from disk. Behaviour:
///
/// - No path resolves (no `HOME`, no override) -> defaults.
/// - Default path exists -> parse it.
/// - Default path is missing -> silently use defaults.
/// - Explicit path (`--config` or `$GASLEAK_CONFIG`) is missing -> error.
/// - Any path that exists but fails to parse -> error.
pub fn load(cli_override: Option<&Path>) -> Result<FileConfig> {
    let Some(source) = resolve(cli_override) else {
        return Ok(FileConfig::default());
    };
    if !source.path.exists() {
        if source.is_explicit {
            return Err(Error::Config(format!(
                "config file not found: {}",
                source.path.display()
            )));
        }
        return Ok(FileConfig::default());
    }
    let raw = std::fs::read_to_string(&source.path).map_err(|e| {
        Error::Config(format!("read {}: {e}", source.path.display()))
    })?;
    toml::from_str::<FileConfig>(&raw).map_err(|e| {
        Error::Config(format!("parse {}: {e}", source.path.display()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_parses_to_defaults() {
        let cfg: FileConfig = toml::from_str("").unwrap();
        assert!(cfg.inactive.low_days.is_none());
        assert!(cfg.inactive.high_days.is_none());
        assert!(cfg.long_lived.age_days.is_none());
        assert!(cfg.warn.window_hours.is_none());
    }

    #[test]
    fn partial_file_parses_only_set_fields() {
        let raw = r#"
            [inactive]
            high_days = 45
            [long_lived]
            age_days = 120
        "#;
        let cfg: FileConfig = toml::from_str(raw).unwrap();
        assert_eq!(cfg.inactive.high_days, Some(45));
        assert!(cfg.inactive.low_days.is_none());
        assert_eq!(cfg.long_lived.age_days, Some(120));
    }

    #[test]
    fn unknown_fields_are_ignored() {
        // Keep the file forward-compatible. A user with a newer config schema
        // should not break older gasleak binaries.
        let raw = r#"
            future_field = 42
            [future_section]
            unused = true
        "#;
        let cfg: FileConfig = toml::from_str(raw).unwrap();
        assert!(cfg.inactive.low_days.is_none());
    }
}
