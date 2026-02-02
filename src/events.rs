//! Shared event types for WebSocket broadcasting.
//!
//! These types are serialized to JSON and sent to connected WebSocket clients.
//! They are produced by the [`TwitchClient`](crate::twitch::TwitchClient) event
//! loop alongside terminal output, providing a parallel data stream for the web UI.

use serde::Serialize;

/// Envelope for all WebSocket events sent to connected clients.
///
/// Uses Serde's internally tagged representation so each JSON message
/// includes a `"type"` field identifying the variant:
///
/// ```json
/// { "type": "ChatMessage", "username": "...", ... }
/// { "type": "Transcription", "transcript": "...", ... }
/// { "type": "ChannelChanged", "channel": "new_channel", ... }
/// ```
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum WebEvent {
    /// A chat message with optional translation data.
    ChatMessage(ChatMessageEvent),
    /// A finalized audio transcription with optional translation.
    Transcription(TranscriptionMessageEvent),
    /// Notification that the active Twitch channel has changed.
    ChannelChanged(ChannelChangedEvent),
}

/// A chat message event broadcast to WebSocket clients.
///
/// Contains the original message, user metadata (badges, color),
/// and translation data if the message was in a non-English language.
#[derive(Debug, Clone, Serialize)]
pub struct ChatMessageEvent {
    /// Display name of the message author.
    pub username: String,
    /// User's Twitch chat color in hex (e.g., "#FF0000"), or null.
    pub color: Option<String>,
    /// Whether the user is the broadcaster.
    pub is_broadcaster: bool,
    /// Whether the user is a moderator.
    pub is_mod: bool,
    /// Whether the user is a VIP.
    pub is_vip: bool,
    /// The original message text.
    pub content: String,
    /// The translated text, if translation was performed.
    pub translation: Option<String>,
    /// Romanization/pronunciation of the source text, if available.
    pub romanization: Option<String>,
    /// Detected or forced source language code (e.g., "ru").
    pub source_language: Option<String>,
    /// Human-readable language name (e.g., "Russian").
    pub language_name: Option<String>,
    /// Reply context if this message is a reply to another.
    pub reply_to: Option<ReplyContext>,
    /// Unix timestamp in milliseconds when this event was created.
    pub timestamp: u64,
}

/// Reply context for threaded chat messages.
#[derive(Debug, Clone, Serialize)]
pub struct ReplyContext {
    /// Display name of the parent message author.
    pub parent_username: String,
    /// The parent message text being replied to.
    pub parent_message: String,
}

/// A finalized audio transcription event broadcast to WebSocket clients.
///
/// Produced when the STT pipeline delivers a final transcript,
/// optionally with translation if the spoken language is non-English.
#[derive(Debug, Clone, Serialize)]
pub struct TranscriptionMessageEvent {
    /// The transcribed text.
    pub transcript: String,
    /// The translated text, if translation was performed.
    pub translation: Option<String>,
    /// Romanization/pronunciation, if available.
    pub romanization: Option<String>,
    /// Source language code (e.g., "ru").
    pub source_language: Option<String>,
    /// Human-readable language name (e.g., "Russian").
    pub language_name: Option<String>,
    /// Unix timestamp in milliseconds when this event was created.
    pub timestamp: u64,
}

/// Event broadcast when the active Twitch channel changes at runtime.
///
/// Sent to all connected WebSocket clients so they can update
/// the displayed channel name and clear stale messages.
#[derive(Debug, Clone, Serialize)]
pub struct ChannelChangedEvent {
    /// The new Twitch channel name (without #).
    pub channel: String,
    /// Unix timestamp in milliseconds when the switch occurred.
    pub timestamp: u64,
}

/// Returns the current time as Unix milliseconds.
///
/// Used to timestamp all outgoing WebSocket events.
pub fn current_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
