//! `sam send <handle> <text>` — one-shot iMessage for debugging.

pub async fn run(handle: String, text: String) -> i32 {
    match sam_imessage::outbound::send_once(&handle, &text).await {
        Ok(()) => {
            eprintln!("sent to {handle}");
            0
        }
        Err(e) => {
            eprintln!("send failed: {e}");
            1
        }
    }
}
