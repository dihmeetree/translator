//! Translation API server.
//!
//! A standalone HTTP API that proxies Google Translate requests.
//! Can be deployed to multiple servers to bypass rate limiting.
//!
//! # Usage
//!
//! ```bash
//! # Start the server
//! translation-api
//!
//! # Or with custom port
//! PORT=8080 translation-api
//! ```
//!
//! # API Endpoints
//!
//! ## GET /translate/:source/:target/:text
//!
//! Translates text and returns both translation and romanization.
//!
//! ### Parameters
//! - `source`: Source language code (e.g., "ru", "ja", "zh", or "auto")
//! - `target`: Target language code (e.g., "en")
//! - `text`: URL-encoded text to translate
//!
//! ### Response
//! ```json
//! {
//!   "translation": "Hello",
//!   "romanization": "privet"
//! }
//! ```
//!
//! ## GET /health
//!
//! Health check endpoint.

use anyhow::Context;
use axum::{extract::Path, http::StatusCode, response::Json, routing::get, Router};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use tower_http::cors::{Any, CorsLayer};
use tracing::{debug, error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Response from the translation API.
#[derive(Debug, Serialize, Deserialize)]
struct TranslationResponse {
    /// The translated text.
    translation: String,

    /// Romanization/pronunciation of the source text (if available).
    #[serde(skip_serializing_if = "Option::is_none")]
    romanization: Option<String>,
}

/// Error response.
#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

/// Shared HTTP client for making requests to Google.
struct AppState {
    client: reqwest::Client,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "translation_api=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Load .env if present
    let _ = dotenvy::dotenv();

    // Get host from environment or use default (127.0.0.1 for local, 0.0.0.0 for Docker)
    let host: std::net::Ipv4Addr = std::env::var("HOST")
        .unwrap_or_else(|_| "127.0.0.1".to_string())
        .parse()
        .map_err(|e| anyhow::anyhow!("HOST must be a valid IPv4 address: {}", e))?;

    // Get port from environment or use default
    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "4000".to_string())
        .parse()
        .map_err(|e| anyhow::anyhow!("PORT must be a valid number: {}", e))?;

    // Create HTTP client
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("Failed to create HTTP client")?;

    let state = std::sync::Arc::new(AppState { client });

    // Configure CORS to allow requests from any origin
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // Build router
    let app = Router::new()
        .route("/health", get(health_check))
        .route("/translate/:source/:target/*text", get(translate))
        .layer(cors)
        .with_state(state);

    let addr = SocketAddr::from((host, port));
    info!("Translation API server starting on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context("Failed to bind to address")?;
    axum::serve(listener, app).await.context("Server error")?;

    Ok(())
}

/// Health check endpoint.
async fn health_check() -> &'static str {
    "OK"
}

/// Translate endpoint.
///
/// GET /translate/:source/:target/:text
async fn translate(
    Path((source, target, text)): Path<(String, String, String)>,
    axum::extract::State(state): axum::extract::State<std::sync::Arc<AppState>>,
) -> Result<Json<TranslationResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Decode the text (it comes URL-encoded from the path)
    let decoded_text = urlencoding::decode(&text).map_err(|e| {
        warn!("Failed to decode text: {}", e);
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("Invalid text encoding: {}", e),
            }),
        )
    })?;

    debug!(
        "Translating '{}' from {} to {}",
        decoded_text, source, target
    );

    // Build Google Translate URL
    let encoded_text = urlencoding::encode(&decoded_text);
    let url = format!(
        "https://translate.googleapis.com/translate_a/single?client=gtx&sl={}&tl={}&dt=t&dt=rm&q={}",
        source, target, encoded_text
    );

    // Make request to Google
    let response = state
        .client
        .get(&url)
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
        )
        .send()
        .await
        .map_err(|e| {
            error!("Failed to send request to Google: {}", e);
            (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse {
                    error: format!("Failed to reach translation service: {}", e),
                }),
            )
        })?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        warn!("Google Translate API error: {} - {}", status, body);
        return Err((
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse {
                error: format!("Translation service returned error: {}", status),
            }),
        ));
    }

    let body = response.text().await.map_err(|e| {
        error!("Failed to read response: {}", e);
        (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse {
                error: format!("Failed to read translation response: {}", e),
            }),
        )
    })?;

    // Parse the JSON response
    let json: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
        error!("Failed to parse JSON: {} - Body: {}", e, body);
        (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse {
                error: format!("Failed to parse translation response: {}", e),
            }),
        )
    })?;

    // Validate response structure: Google Translate returns an array at index 0
    let segments = json.get(0).and_then(|v| v.as_array()).ok_or_else(|| {
        warn!("Unexpected Google Translate response structure: missing segment array at index 0");
        (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse {
                error: "Invalid translation response format: missing segments".to_string(),
            }),
        )
    })?;

    // Extract all translation segments from [0][*][0]
    // Google splits translations into multiple segments, we need to concatenate them
    let mut translation = String::new();
    let mut romanization: Option<String> = None;

    for segment in segments {
        // Each segment has translation at index 0
        if let Some(text) = segment.get(0).and_then(|v| v.as_str()) {
            translation.push_str(text);
        }
        // Check for romanization at index 3 (usually in the last segment with romanization)
        if let Some(rom) = segment.get(3).and_then(|v| v.as_str()) {
            if !rom.is_empty() {
                romanization = Some(rom.to_string());
            }
        }
    }

    if translation.is_empty() {
        warn!(
            "Google Translate returned empty translation for text: {}",
            decoded_text
        );
        return Err((
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse {
                error: "Empty translation received from Google".to_string(),
            }),
        ));
    }

    debug!(
        "Translated '{}' -> '{}' (romanization: {:?})",
        decoded_text, translation, romanization
    );

    Ok(Json(TranslationResponse {
        translation,
        romanization,
    }))
}
