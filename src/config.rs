//! Configuration module for the Twitch Chat Translator.
//!
//! Handles loading configuration from environment variables and .env files.

use crate::translator::TranslationBackend;
use anyhow::{Context, Result};

/// Application configuration loaded from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    /// The Twitch channel to connect to (without the # prefix).
    pub channel: String,

    /// OAuth token for Twitch IRC authentication.
    /// Can be obtained from https://twitchapps.com/tmi/
    /// For anonymous read-only access, use "SCHMOOPIIE" as username with no token.
    pub oauth_token: Option<String>,

    /// Twitch username for IRC authentication.
    /// For anonymous access, use "justinfan" followed by random numbers.
    pub username: String,

    /// URL of the translation API endpoint.
    pub translation_api_url: String,

    /// Which translation backend to use (google, proxy, or libretranslate).
    pub translation_backend: TranslationBackend,

    /// Minimum confidence threshold for language detection (0.0 to 1.0).
    /// Messages with detection confidence below this will not be translated.
    pub detection_confidence_threshold: f64,

    /// Default source language code (e.g., "ru", "es", "fr").
    /// When set, all messages will be translated from this language,
    /// bypassing language detection entirely.
    pub default_language: Option<String>,

    /// Maximum size of the translation cache in megabytes.
    /// When exceeded, oldest entries are purged automatically.
    pub cache_size_mb: u64,

    /// Deepgram API key for speech-to-text transcription.
    /// When set, enables real-time audio transcription of the stream.
    /// If not set, transcription is disabled and the app behaves as before.
    pub deepgram_api_key: Option<String>,

    /// Deepgram model to use for transcription (e.g., "nova-2", "nova-2-general").
    pub deepgram_model: String,

    /// Language hint for Deepgram speech-to-text.
    /// Defaults to `DEFAULT_LANGUAGE` if set, otherwise "multi" for auto-detection.
    /// Can be overridden explicitly with `STT_LANGUAGE`.
    pub stt_language: String,

    /// Port for the web UI HTTP/WebSocket server.
    /// Defaults to 3000. The web UI is always started and serves the
    /// live translation dashboard at `http://localhost:{web_port}`.
    pub web_port: u16,

    /// Timeout in seconds for the IRC WebSocket read loop.
    /// If no message is received within this duration, a keepalive PING is sent.
    /// Lower values detect dead connections faster but generate more keepalive traffic.
    /// Defaults to 300 seconds (5 minutes).
    pub irc_timeout_secs: u64,
}

impl Config {
    /// Loads configuration from environment variables.
    ///
    /// Required environment variables:
    /// - `TWITCH_CHANNEL`: The channel to join
    ///
    /// Optional environment variables:
    /// - `TWITCH_OAUTH_TOKEN`: OAuth token (omit for anonymous access)
    /// - `TWITCH_USERNAME`: Username (defaults to anonymous)
    /// - `TRANSLATION_API_URL`: Translation API server URL (default: http://localhost:4000)
    /// - `TRANSLATION_BACKEND`: "google" (default) or "libretranslate"
    /// - `DETECTION_CONFIDENCE`: Minimum confidence for language detection (default: 0.5)
    /// - `DEFAULT_LANGUAGE`: Source language code to translate all messages from (e.g., "ru")
    /// - `CACHE_SIZE_MB`: Maximum cache size in megabytes (default: 50)
    /// - `DEEPGRAM_API_KEY`: Deepgram API key to enable stream audio transcription
    /// - `DEEPGRAM_MODEL`: Deepgram model for transcription (default: "nova-2")
    /// - `STT_LANGUAGE`: Language hint for speech-to-text (default: DEFAULT_LANGUAGE or "multi")
    pub fn load() -> Result<Self> {
        // Load .env file if present (ignore errors if not found)
        let _ = dotenvy::dotenv();

        let channel = std::env::var("TWITCH_CHANNEL")
            .context("TWITCH_CHANNEL environment variable is required")?;

        // For anonymous access, use justinfan + random number
        let username = std::env::var("TWITCH_USERNAME")
            .unwrap_or_else(|_| format!("justinfan{}", rand_number()));

        let oauth_token = std::env::var("TWITCH_OAUTH_TOKEN").ok();

        // Parse translation backend (default to Google)
        let translation_backend: TranslationBackend = std::env::var("TRANSLATION_BACKEND")
            .map(|s| s.parse().unwrap_or_default())
            .unwrap_or_default();

        // Set default URL based on backend
        let default_url = match translation_backend {
            TranslationBackend::Google => "http://localhost:4000",
            TranslationBackend::LibreTranslate => "http://localhost:5000/translate",
        };

        let translation_api_url =
            std::env::var("TRANSLATION_API_URL").unwrap_or_else(|_| default_url.to_string());

        let detection_confidence_threshold: f64 = std::env::var("DETECTION_CONFIDENCE")
            .unwrap_or_else(|_| "0.5".to_string())
            .parse()
            .context("DETECTION_CONFIDENCE must be a valid number between 0.0 and 1.0")?;

        let default_language = std::env::var("DEFAULT_LANGUAGE").ok();

        let cache_size_mb: u64 = std::env::var("CACHE_SIZE_MB")
            .unwrap_or_else(|_| "50".to_string())
            .parse()
            .context("CACHE_SIZE_MB must be a valid positive number")?;

        let deepgram_api_key = std::env::var("DEEPGRAM_API_KEY").ok();

        let deepgram_model =
            std::env::var("DEEPGRAM_MODEL").unwrap_or_else(|_| "nova-2".to_string());

        let stt_language = std::env::var("STT_LANGUAGE").unwrap_or_else(|_| {
            default_language
                .clone()
                .unwrap_or_else(|| "multi".to_string())
        });

        let web_port: u16 = std::env::var("WEB_PORT")
            .unwrap_or_else(|_| "3000".to_string())
            .parse()
            .context("WEB_PORT must be a valid port number")?;

        let irc_timeout_secs: u64 = std::env::var("IRC_TIMEOUT_SECS")
            .unwrap_or_else(|_| "300".to_string())
            .parse()
            .context("IRC_TIMEOUT_SECS must be a valid positive number")?;

        Ok(Config {
            channel,
            oauth_token,
            username,
            translation_api_url,
            translation_backend,
            detection_confidence_threshold,
            default_language,
            cache_size_mb,
            deepgram_api_key,
            deepgram_model,
            stt_language,
            web_port,
            irc_timeout_secs,
        })
    }
}

/// Generates a random number for anonymous username.
fn rand_number() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (duration.as_nanos() % 99999) as u32
}
