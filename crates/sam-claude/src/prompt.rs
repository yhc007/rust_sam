//! System prompt loader for Sam.

use tracing::debug;

/// Default Korean system prompt used when `~/.sam/prompts/system.md` is missing.
const DEFAULT_PROMPT: &str = "\
너는 Sam이야. Paul의 개인 AI 비서로, 한국어로 대화해. \
친근하고 도움이 되는 톤으로 답변하되, 간결하게 이야기해. \
Paul이 영어로 말하면 영어로 답하고, 한국어로 말하면 한국어로 답해.";

/// Load the system prompt from `~/.sam/prompts/system.md`.
///
/// If the file does not exist, returns a sensible default Korean prompt.
pub fn load_system_prompt() -> String {
    let path = sam_core::prompts_dir().join("system.md");
    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            let trimmed = contents.trim().to_string();
            if trimmed.is_empty() {
                debug!("system.md is empty — using default prompt");
                DEFAULT_PROMPT.to_string()
            } else {
                debug!(path = %path.display(), "loaded system prompt from file");
                trimmed
            }
        }
        Err(_) => {
            debug!(path = %path.display(), "system.md not found — using default prompt");
            DEFAULT_PROMPT.to_string()
        }
    }
}
