//! Outbound iMessage sending. M1 is dry-run only; no `osascript` is spawned.

use tracing::info;

/// Build the AppleScript one-liner that `osascript -e` would execute to send
/// a message to the given handle via Messages.app.
///
/// Both arguments are embedded using AppleScript's escape conventions (double
/// quote and backslash). Newlines in `body` are preserved as-is — AppleScript
/// string literals accept literal newlines.
pub fn build_applescript(handle: &str, body: &str) -> String {
    format!(
        "tell application \"Messages\"\n\
        \tset targetService to first service whose service type = iMessage\n\
        \tset targetBuddy to buddy \"{h}\" of targetService\n\
        \tsend \"{b}\" to targetBuddy\n\
        end tell",
        h = escape_applescript(handle),
        b = escape_applescript(body),
    )
}

/// Log the message that *would* be sent, without actually sending. Used by
/// M1's `sam status`-driven diagnostics and tests.
pub fn dry_send(handle: &str, body: &str) {
    info!(
        target = "sam_imessage::sender",
        handle = handle,
        bytes = body.len(),
        "dry-send (M1): osascript not invoked",
    );
    eprintln!("[sam-imessage dry-send] → {handle}: {body}");
}

/// Build an AppleScript that sends a message via Messages.app, with proper
/// handling of newlines (converted to `return` expressions).
///
/// Uses `chat id "any;-;{handle}"` which works on macOS 26 (Tahoe) and later.
/// Earlier macOS versions used `buddy ... of service`, but that API is broken
/// in macOS 26.
pub fn build_applescript_live(handle: &str, body: &str) -> String {
    let h = escape_applescript(handle);
    let b = escape_applescript(body).replace('\n', "\" & return & \"");
    format!(
        "tell application \"Messages\"\n\
        \tset targetChat to chat id \"any;-;{h}\"\n\
        \tsend \"{b}\" to targetChat\n\
        end tell"
    )
}

/// Build an AppleScript that sends a file attachment via Messages.app.
/// Sends the file first, then the caption text (if non-empty).
pub fn build_applescript_attachment(handle: &str, file_path: &str, caption: &str) -> String {
    let h = escape_applescript(handle);
    let f = escape_applescript(file_path);
    let mut script = format!(
        "tell application \"Messages\"\n\
        \tset targetChat to chat id \"any;-;{h}\"\n\
        \tset theFile to POSIX file \"{f}\"\n\
        \tsend theFile to targetChat\n"
    );
    if !caption.is_empty() {
        let b = escape_applescript(caption).replace('\n', "\" & return & \"");
        script.push_str(&format!("\tsend \"{b}\" to targetChat\n"));
    }
    script.push_str("end tell");
    script
}

pub fn escape_applescript(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_quotes_and_backslashes() {
        let script = build_applescript("+15551234567", "hi \"world\" \\");
        assert!(script.contains("\\\"world\\\""));
        assert!(script.contains("\\\\"));
    }

    #[test]
    fn live_script_handles_newlines() {
        let script = build_applescript_live("+15551234567", "line1\nline2");
        assert!(script.contains("\" & return & \""));
        assert!(!script.contains('\n') || script.contains("return"));
    }

    #[test]
    fn live_script_escapes_quotes() {
        let script = build_applescript_live("+15551234567", "say \"hello\"");
        assert!(script.contains("\\\"hello\\\""));
    }
}
