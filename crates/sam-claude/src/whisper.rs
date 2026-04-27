//! Whisper STT (Speech-to-Text) integration for voice memo processing.
//!
//! Transcribes audio files using OpenAI-compatible Whisper API endpoints.
//! Supports local whisper.cpp server or OpenAI's hosted API.

use std::path::Path;

use reqwest::multipart;
use serde::Deserialize;
use tracing::{info, warn};

use sam_core::SamConfig;

/// Configuration for the Whisper STT service.
/// Uses config.whisper or falls back to sensible defaults.
const DEFAULT_WHISPER_URL: &str = "http://localhost:8080/v1/audio/transcriptions";
const MAX_AUDIO_SIZE_BYTES: u64 = 25 * 1024 * 1024; // 25 MB

#[derive(Debug, Deserialize)]
struct WhisperResponse {
    text: String,
}

/// Transcribe an audio file to text using Whisper API.
///
/// Supports: .m4a, .caf, .mp3, .wav, .ogg, .webm
/// Returns the transcribed text or an error.
pub async fn transcribe_audio(
    audio_path: &Path,
    config: &SamConfig,
) -> Result<String, String> {
    // Validate file exists and isn't too large.
    let metadata = std::fs::metadata(audio_path)
        .map_err(|e| format!("오디오 파일 접근 실패: {e}"))?;

    if metadata.len() > MAX_AUDIO_SIZE_BYTES {
        return Err(format!(
            "오디오 파일이 너무 큼 ({:.1}MB > 25MB)",
            metadata.len() as f64 / 1_048_576.0
        ));
    }

    let extension = audio_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("m4a");

    // Determine the Whisper endpoint URL.
    let whisper_url = config
        .whisper
        .url
        .as_deref()
        .unwrap_or(DEFAULT_WHISPER_URL);

    // Load the audio file.
    let file_bytes = std::fs::read(audio_path)
        .map_err(|e| format!("오디오 파일 읽기 실패: {e}"))?;

    let filename = audio_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("audio.{extension}"));

    info!(
        path = %audio_path.display(),
        size_kb = metadata.len() / 1024,
        "transcribing audio"
    );

    // Build multipart form.
    let file_part = multipart::Part::bytes(file_bytes)
        .file_name(filename)
        .mime_str(&mime_for_extension(extension))
        .map_err(|e| format!("MIME 설정 실패: {e}"))?;

    let form = multipart::Form::new()
        .part("file", file_part)
        .text("model", config.whisper.model.clone().unwrap_or_else(|| "whisper-1".to_string()))
        .text("language", "ko")
        .text("response_format", "json");

    let client = reqwest::Client::new();
    let mut req = client.post(whisper_url).multipart(form);

    // Add API key if configured.
    if let Some(ref key_source) = config.whisper.api_key_source {
        let key = crate::tools::load_key_from_source_pub(key_source)?;
        req = req.header("Authorization", format!("Bearer {key}"));
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("Whisper API 요청 실패: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Whisper API 에러 HTTP {status}: {body}"));
    }

    let result: WhisperResponse = resp
        .json()
        .await
        .map_err(|e| format!("Whisper 응답 파싱 실패: {e}"))?;

    if result.text.is_empty() {
        warn!(path = %audio_path.display(), "whisper returned empty transcription");
        return Ok("[음성을 인식하지 못했습니다]".to_string());
    }

    info!(
        path = %audio_path.display(),
        text_len = result.text.len(),
        "transcription complete"
    );

    Ok(result.text)
}

/// Map file extension to MIME type.
fn mime_for_extension(ext: &str) -> String {
    match ext.to_lowercase().as_str() {
        "m4a" => "audio/mp4",
        "caf" => "audio/x-caf",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "webm" => "audio/webm",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Check if a file extension is a supported audio format.
pub fn is_audio_file(extension: &str) -> bool {
    matches!(
        extension.to_lowercase().as_str(),
        "m4a" | "caf" | "mp3" | "wav" | "ogg" | "webm" | "aac" | "flac" | "amr"
    )
}
