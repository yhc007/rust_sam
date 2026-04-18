//! API key loading for the Claude API.

use sam_core::LlmConfig;
use tracing::debug;

/// Load the Anthropic API key using the following resolution order:
///
/// 1. `ANTHROPIC_API_KEY` environment variable
/// 2. `config.api_key_source` starting with `env:` — read that env var
/// 3. `config.api_key_source` starting with `file:` — read that file
/// 4. Otherwise return an error
///
/// The actual key value is **never** included in log or error messages.
pub fn load_api_key(config: &LlmConfig) -> anyhow::Result<String> {
    // 1. Try ANTHROPIC_API_KEY env var first.
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        let key = key.trim().to_string();
        if !key.is_empty() {
            debug!("API key loaded from ANTHROPIC_API_KEY env var");
            return Ok(key);
        }
    }

    // 2-3. Try api_key_source from config.
    if let Some(source) = config.api_key_source.as_deref() {
        if let Some(var_name) = source.strip_prefix("env:") {
            let key = std::env::var(var_name)
                .map_err(|_| anyhow::anyhow!("API key env var not set"))?
                .trim()
                .to_string();
            if key.is_empty() {
                anyhow::bail!("API key env var is empty");
            }
            debug!("API key loaded from env var specified in config");
            return Ok(key);
        }

        if let Some(file_path) = source.strip_prefix("file:") {
            let expanded = sam_core::expand_tilde(file_path);
            let key = std::fs::read_to_string(&expanded)
                .map_err(|_| anyhow::anyhow!("API key file not readable"))?
                .trim()
                .to_string();
            if key.is_empty() {
                anyhow::bail!("API key file is empty");
            }
            debug!("API key loaded from file specified in config");
            return Ok(key);
        }
    }

    // 4. Nothing found.
    Err(sam_core::SamError::ApiKeyMissing(
        "no API key found — set ANTHROPIC_API_KEY or configure api_key_source".to_string(),
    )
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: Tests that manipulate ANTHROPIC_API_KEY are inherently racy when
    // run in parallel.  We only test config-driven sources (env:CUSTOM_VAR
    // and file:) which use unique names and are safe to run concurrently.

    #[test]
    fn loads_from_custom_env_source() {
        let key = "sk-ant-custom-456";
        std::env::set_var("SAM_TEST_CLAUDE_KEY_CUSTOM", key);

        let mut config = LlmConfig::default();
        config.api_key_source = Some("env:SAM_TEST_CLAUDE_KEY_CUSTOM".to_string());

        let result = load_api_key(&config).expect("should load key from custom env");
        assert_eq!(result, key);

        std::env::remove_var("SAM_TEST_CLAUDE_KEY_CUSTOM");
    }

    #[test]
    fn loads_from_file_source() {
        let key = "sk-ant-file-789";
        let tmp = std::env::temp_dir().join("sam-test-api-key-file");
        std::fs::write(&tmp, format!("  {key}  \n")).unwrap();

        let mut config = LlmConfig::default();
        config.api_key_source = Some(format!("file:{}", tmp.display()));

        let result = load_api_key(&config).expect("should load key from file");
        assert_eq!(result, key);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn error_when_no_source_configured() {
        // Config with no api_key_source and a unique env var that won't exist.
        let mut config = LlmConfig::default();
        config.api_key_source = Some("env:SAM_TEST_NONEXISTENT_VAR_12345".to_string());

        let result = load_api_key(&config);
        assert!(result.is_err());
    }
}
