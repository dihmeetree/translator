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

        Ok(Config {
            channel,
            oauth_token,
            username,
            translation_api_url,
            translation_backend,
            detection_confidence_threshold,
            default_language,
            cache_size_mb,
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
