//! Path helpers for the Sam home directory.

use std::path::{Path, PathBuf};

/// Return the root of the Sam home directory, honoring `$SAM_HOME` if set,
/// otherwise defaulting to `~/.sam`.
pub fn sam_home() -> PathBuf {
    if let Ok(env) = std::env::var("SAM_HOME") {
        return PathBuf::from(expand_tilde(&env));
    }
    match dirs::home_dir() {
        Some(home) => home.join(".sam"),
        None => PathBuf::from(".sam"),
    }
}

/// Path to the Sam config file (`~/.sam/config.toml`).
pub fn config_path() -> PathBuf {
    sam_home().join("config.toml")
}

/// Path to the Sam prompts directory (`~/.sam/prompts`).
pub fn prompts_dir() -> PathBuf {
    sam_home().join("prompts")
}

/// Path to the Sam external-tools directory (`~/.sam/tools`).
pub fn tools_dir() -> PathBuf {
    sam_home().join("tools")
}

/// Path to the Sam state directory (`~/.sam/state`).
pub fn state_dir() -> PathBuf {
    sam_home().join("state")
}

/// Expand a leading `~` in a path string to the user's home directory.
///
/// Non-tilde inputs pass through unchanged. A `~` with no following character
/// expands to just the home directory. A `~/` prefix expands to `$HOME/...`.
pub fn expand_tilde(input: &str) -> String {
    if input == "~" {
        return dirs::home_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| input.to_string());
    }
    if let Some(rest) = input.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    input.to_string()
}

/// Expand tilde then convert to a `PathBuf`.
pub fn expand_tilde_path(input: &str) -> PathBuf {
    PathBuf::from(expand_tilde(input))
}

/// Return `input` joined under `base` if it is relative, or `input` as-is
/// if it is absolute. Used for resolving config-relative paths.
pub fn resolve_under(base: &Path, input: impl AsRef<Path>) -> PathBuf {
    let p = input.as_ref();
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}
