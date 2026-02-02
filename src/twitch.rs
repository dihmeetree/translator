//! Twitch IRC client module.
//!
//! Handles connecting to Twitch chat via IRC over WebSocket and processing messages.

use crate::config::Config;
use crate::transcription::{TranscriptionConfig, TranscriptionEvent};
use crate::translator::Translator;
use any_ascii::any_ascii;
use anyhow::{Context, Result};
use colored::Colorize;
use crossterm::{cursor, execute, terminal};
use futures_util::{SinkExt, StreamExt};
use std::io::stdout;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use terminal_size::{terminal_size, Width};
use tokio::sync::{mpsc, watch};
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

/// Twitch IRC WebSocket URL.
const TWITCH_IRC_WSS: &str = "wss://irc-ws.chat.twitch.tv:443";

/// Twitch IRC client that connects to a channel and processes messages.
pub struct TwitchClient {
    /// Application configuration.
    config: Config,

    /// Translation service.
    translator: Translator,

    /// WebSocket connection.
    ws_stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,

    /// Receiver for transcription events (None if transcription disabled).
    stt_rx: Option<mpsc::Receiver<TranscriptionEvent>>,

    /// Shutdown signal sender for the transcription pipeline.
    stt_shutdown_tx: Option<watch::Sender<bool>>,

    /// Tracks the current interim transcript for overwrite display.
    current_interim: Option<String>,

    /// Tracks the last finalized transcript to deduplicate consecutive
    /// identical `is_final` results from Deepgram.
    last_final_transcript: Option<String>,

    /// When the current interim was last updated.
    /// Used to auto-commit stale interims that Deepgram never finalized.
    interim_set_at: Option<tokio::time::Instant>,
}

/// Parsed chat message from Twitch IRC.
#[derive(Debug, Clone)]
struct ChatMessage {
    /// Username of the message sender.
    username: String,

    /// The message content.
    content: String,

    /// Optional display name with color information.
    display_name: Option<String>,

    /// User color in hex format (e.g., "#FF0000").
    color: Option<String>,

    /// Whether the user is a moderator.
    is_mod: bool,

    /// Whether the user is the broadcaster (channel owner).
    is_broadcaster: bool,

    /// Whether the user is a VIP.
    is_vip: bool,

    /// Reply information if this message is a reply to another.
    reply_to: Option<ReplyInfo>,
}

/// Information about the parent message being replied to.
#[derive(Debug, Clone)]
struct ReplyInfo {
    /// Display name of the user being replied to.
    parent_display_name: String,

    /// The message body being replied to.
    parent_msg_body: String,
}

/// Duration to wait after the last interim update before auto-committing it.
/// Handles cases where Deepgram doesn't send `is_final` for a segment.
const INTERIM_COMMIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Actions produced by the main event loop's `tokio::select!` branches.
///
/// This enum decouples the `select!` block (which borrows individual fields)
/// from the action handling (which needs `&mut self`), avoiding borrow conflicts.
enum LoopAction {
    /// An IRC text message was received.
    IrcMessage(String),
    /// An IRC WebSocket error occurred.
    IrcError(String),
    /// The IRC WebSocket connection was closed.
    IrcClosed,
    /// The IRC keepalive timeout expired.
    IrcTimeout,
    /// A transcription event was received from the STT pipeline.
    SttEvent(TranscriptionEvent),
    /// The transcription channel was closed (pipeline exited).
    SttClosed,
    /// An interim transcript has been sitting for too long without being finalized.
    SttInterimTimeout,
}

impl TwitchClient {
    /// Creates a new TwitchClient and connects to the IRC server.
    ///
    /// # Arguments
    ///
    /// * `config` - Application configuration including channel and credentials.
    /// * `translator` - Translation service for non-English messages.
    pub async fn new(config: Config, translator: Translator) -> Result<Self> {
        info!("Connecting to Twitch IRC...");

        let (ws_stream, _) = connect_async(TWITCH_IRC_WSS)
            .await
            .context("Failed to connect to Twitch IRC WebSocket")?;

        let mut client = Self {
            config: config.clone(),
            translator,
            ws_stream,
            stt_rx: None,
            stt_shutdown_tx: None,
            current_interim: None,
            last_final_transcript: None,
            interim_set_at: None,
        };

        client.authenticate().await?;
        client.join_channel().await?;

        // Start transcription pipeline if Deepgram API key is configured
        if let Some(ref api_key) = config.deepgram_api_key {
            let (event_tx, event_rx) = mpsc::channel::<TranscriptionEvent>(64);
            let (shutdown_tx, shutdown_rx) = watch::channel(false);

            let stt_config = TranscriptionConfig {
                api_key: api_key.clone(),
                channel: config.channel.clone(),
                language: config.stt_language.clone(),
                model: config.deepgram_model.clone(),
            };

            tokio::spawn(crate::transcription::run_pipeline(
                stt_config,
                event_tx,
                shutdown_rx,
            ));

            client.stt_rx = Some(event_rx);
            client.stt_shutdown_tx = Some(shutdown_tx);

            info!(
                "Speech-to-text enabled (model: {}, language: {})",
                config.deepgram_model, config.stt_language
            );
        }

        Ok(client)
    }

    /// Authenticates with the Twitch IRC server.
    async fn authenticate(&mut self) -> Result<()> {
        // Request additional IRC capabilities for tags (colors, badges, etc.)
        self.send_raw("CAP REQ :twitch.tv/tags twitch.tv/commands")
            .await?;

        // Send OAuth token (or anonymous token)
        let pass = self
            .config
            .oauth_token
            .as_ref()
            .map(|t| format!("oauth:{}", t))
            .unwrap_or_else(|| "SCHMOOPIIE".to_string());

        self.send_raw(&format!("PASS {}", pass)).await?;
        self.send_raw(&format!("NICK {}", self.config.username))
            .await?;

        info!("Authenticated as {}", self.config.username);
        Ok(())
    }

    /// Joins the configured channel.
    async fn join_channel(&mut self) -> Result<()> {
        self.send_raw(&format!("JOIN #{}", self.config.channel))
            .await?;

        info!("Joined channel #{}", self.config.channel);
        Ok(())
    }

    /// Sends a raw IRC message.
    async fn send_raw(&mut self, message: &str) -> Result<()> {
        debug!("Sending: {}", message);
        self.ws_stream
            .send(Message::Text(format!("{}\r\n", message)))
            .await
            .context("Failed to send IRC message")?;
        Ok(())
    }

    /// Main loop that processes incoming IRC messages and transcription events.
    ///
    /// When transcription is enabled, uses `tokio::select!` to multiplex
    /// IRC WebSocket messages and transcription events from the STT pipeline.
    /// When disabled, processes only IRC messages as before.
    pub async fn run(&mut self) -> Result<()> {
        print_thick_divider();
        println!("{}", "Waiting for messages...".bright_black().italic());

        loop {
            // Produce a LoopAction from select! without holding &mut self borrows,
            // then handle it separately to allow full &mut self access.
            // Compute the interim commit deadline before entering select!
            // so we don't borrow self inside the macro.
            let interim_deadline = self.interim_set_at.map(|t| t + INTERIM_COMMIT_TIMEOUT);

            let action = if let Some(ref mut stt_rx) = self.stt_rx {
                tokio::select! {
                    ws_result = timeout(Duration::from_secs(300), self.ws_stream.next()) => {
                        match ws_result {
                            Ok(Some(Ok(Message::Text(text)))) => LoopAction::IrcMessage(text),
                            Ok(Some(Ok(_))) => continue,
                            Ok(Some(Err(e))) => LoopAction::IrcError(e.to_string()),
                            Ok(None) => LoopAction::IrcClosed,
                            Err(_) => LoopAction::IrcTimeout,
                        }
                    }
                    stt_event = stt_rx.recv() => {
                        match stt_event {
                            Some(event) => LoopAction::SttEvent(event),
                            None => LoopAction::SttClosed,
                        }
                    }
                    // Auto-commit stale interims that Deepgram never finalized
                    _ = async {
                        match interim_deadline {
                            Some(deadline) => tokio::time::sleep_until(deadline).await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {
                        LoopAction::SttInterimTimeout
                    }
                }
            } else {
                match timeout(Duration::from_secs(300), self.ws_stream.next()).await {
                    Ok(Some(Ok(Message::Text(text)))) => LoopAction::IrcMessage(text),
                    Ok(Some(Ok(_))) => continue,
                    Ok(Some(Err(e))) => LoopAction::IrcError(e.to_string()),
                    Ok(None) => LoopAction::IrcClosed,
                    Err(_) => LoopAction::IrcTimeout,
                }
            };

            match action {
                LoopAction::IrcMessage(text) => {
                    for line in text.lines() {
                        self.handle_irc_message(line).await?;
                    }
                }
                LoopAction::IrcError(e) => {
                    error!("WebSocket error: {}", e);
                    break;
                }
                LoopAction::IrcClosed => {
                    warn!("WebSocket connection closed");
                    break;
                }
                LoopAction::IrcTimeout => {
                    debug!("Sending keepalive PING");
                    self.send_raw("PING :tmi.twitch.tv").await?;
                }
                LoopAction::SttEvent(event) => {
                    self.handle_transcription_event(event).await;
                }
                LoopAction::SttClosed => {
                    warn!("Transcription channel closed");
                    self.stt_rx = None;
                }
                LoopAction::SttInterimTimeout => {
                    // Deepgram didn't finalize this segment - commit the interim
                    // as permanent so it doesn't get lost when new speech arrives.
                    if let Some(transcript) = self.current_interim.clone() {
                        debug!("Auto-committing stale interim: {}", transcript);
                        self.clear_interim_transcript();
                        self.last_final_transcript = Some(transcript.clone());
                        self.display_final_transcript(&transcript, 0.0).await;
                    }
                }
            }
        }

        // Signal transcription pipeline to shut down
        if let Some(ref shutdown_tx) = self.stt_shutdown_tx {
            let _ = shutdown_tx.send(true);
        }

        Ok(())
    }

    /// Handles a single IRC message line.
    async fn handle_irc_message(&mut self, line: &str) -> Result<()> {
        debug!("Received: {}", line);

        // Handle PING/PONG for keepalive
        if line.starts_with("PING") {
            self.send_raw("PONG :tmi.twitch.tv").await?;
            return Ok(());
        }

        // Parse PRIVMSG (chat messages)
        if let Some(chat_msg) = self.parse_privmsg(line) {
            self.process_chat_message(chat_msg).await;
        }

        Ok(())
    }

    /// Parses a PRIVMSG IRC message into a ChatMessage struct.
    fn parse_privmsg(&self, line: &str) -> Option<ChatMessage> {
        // Format: @tags :user!user@user.tmi.twitch.tv PRIVMSG #channel :message
        if !line.contains("PRIVMSG") {
            return None;
        }

        let mut tags_str = "";
        let mut rest = line;

        // Extract tags if present
        if line.starts_with('@') {
            let parts: Vec<&str> = line.splitn(2, ' ').collect();
            if parts.len() == 2 {
                tags_str = parts[0];
                rest = parts[1];
            }
        }

        // Parse username from :user!user@user.tmi.twitch.tv
        let username = rest.split('!').next()?.trim_start_matches(':').to_string();

        // Extract message content after the channel name
        let privmsg_idx = rest.find("PRIVMSG")?;
        let after_privmsg = &rest[privmsg_idx + 7..];
        let msg_start = after_privmsg.find(':')?;
        let content = after_privmsg[msg_start + 1..].trim().to_string();

        // Parse tags for display name, color, badges, and reply info
        let mut display_name = None;
        let mut color = None;
        let mut is_mod = false;
        let mut is_broadcaster = false;
        let mut is_vip = false;
        let mut reply_parent_display_name = None;
        let mut reply_parent_msg_body = None;

        for tag in tags_str.trim_start_matches('@').split(';') {
            let parts: Vec<&str> = tag.splitn(2, '=').collect();
            if parts.len() == 2 {
                match parts[0] {
                    "display-name" if !parts[1].is_empty() => {
                        display_name = Some(parts[1].to_string());
                    }
                    "color" if !parts[1].is_empty() => {
                        color = Some(parts[1].to_string());
                    }
                    "mod" => {
                        is_mod = parts[1] == "1";
                    }
                    "badges" => {
                        // Parse badges like "broadcaster/1,subscriber/12"
                        let badges = parts[1];
                        if badges.contains("broadcaster") {
                            is_broadcaster = true;
                        }
                        if badges.contains("vip") {
                            is_vip = true;
                        }
                        if badges.contains("moderator") {
                            is_mod = true;
                        }
                    }
                    "reply-parent-display-name" if !parts[1].is_empty() => {
                        reply_parent_display_name = Some(parts[1].to_string());
                    }
                    "reply-parent-msg-body" if !parts[1].is_empty() => {
                        // Unescape the message body (spaces are \s, etc.)
                        let unescaped = parts[1]
                            .replace("\\s", " ")
                            .replace("\\n", "\n")
                            .replace("\\r", "\r")
                            .replace("\\:", ":")
                            .replace("\\\\", "\\");
                        reply_parent_msg_body = Some(unescaped);
                    }
                    _ => {}
                }
            }
        }

        // Build reply info if both parent name and body are present
        let reply_to = match (reply_parent_display_name, reply_parent_msg_body) {
            (Some(name), Some(body)) => Some(ReplyInfo {
                parent_display_name: name,
                parent_msg_body: body,
            }),
            _ => None,
        };

        Some(ChatMessage {
            username,
            content,
            display_name,
            color,
            is_mod,
            is_broadcaster,
            is_vip,
            reply_to,
        })
    }

    /// Processes a chat message, detecting language and translating if needed.
    async fn process_chat_message(&mut self, msg: ChatMessage) {
        let display = msg
            .display_name
            .as_ref()
            .unwrap_or(&msg.username)
            .to_string();

        // Build badge prefix
        let badge_prefix = build_badge_prefix(msg.is_broadcaster, msg.is_mod, msg.is_vip);

        // Reset interim state so a stale timeout doesn't fire during chat output
        self.clear_interim_transcript();

        // Print divider before each message
        print_divider();

        // Display reply context if this is a reply
        if let Some(ref reply) = msg.reply_to {
            self.display_reply_context(reply);
        }

        // Check if a default language is set - if so, translate all messages
        if let Some(ref default_lang) = self.config.default_language {
            self.display_message_with_forced_translation(
                &badge_prefix,
                &display,
                &msg.content,
                &msg.color,
                default_lang,
            )
            .await;
            return;
        }

        // Detect language
        let detection = self.translator.detect_language(&msg.content);

        match detection {
            Some(ref det)
                if !det.is_english
                    && det.confidence >= self.config.detection_confidence_threshold =>
            {
                // Non-English message detected - translate it
                self.display_message_with_translation(
                    &badge_prefix,
                    &display,
                    &msg.content,
                    &msg.color,
                    det,
                )
                .await;
            }
            _ => {
                // English or low confidence - just display normally
                self.display_message(&badge_prefix, &display, &msg.content, &msg.color);
            }
        }
    }

    /// Displays the context of a reply (the message being replied to).
    fn display_reply_context(&self, reply: &ReplyInfo) {
        // Truncate long messages (by character count, not bytes)
        let max_chars = 50;
        let truncated_body: String = if reply.parent_msg_body.chars().count() > max_chars {
            let truncated: String = reply.parent_msg_body.chars().take(max_chars).collect();
            format!("{}...", truncated)
        } else {
            reply.parent_msg_body.clone()
        };

        println!(
            "  {} {} {}",
            "┌─ replying to".bright_black(),
            reply.parent_display_name.bright_blue(),
            format!("\"{}\"", truncated_body).bright_black().italic()
        );
    }

    /// Displays a regular chat message with colored output.
    fn display_message(
        &self,
        badge_prefix: &str,
        username: &str,
        content: &str,
        color: &Option<String>,
    ) {
        let colored_name = colorize_username(username, color);
        println!("{}{}: {}", badge_prefix, colored_name, content);
    }

    /// Displays a message with its translation.
    async fn display_message_with_translation(
        &self,
        badge_prefix: &str,
        username: &str,
        content: &str,
        color: &Option<String>,
        detection: &crate::translator::DetectionResult,
    ) {
        self.display_translated_message(
            badge_prefix,
            username,
            content,
            color,
            &detection.language,
            &detection.language_name,
        )
        .await;
    }

    /// Displays a message with forced translation from a specified source language.
    ///
    /// Used when DEFAULT_LANGUAGE is set to translate all messages regardless of detection.
    async fn display_message_with_forced_translation(
        &self,
        badge_prefix: &str,
        username: &str,
        content: &str,
        color: &Option<String>,
        source_lang: &str,
    ) {
        self.display_translated_message(
            badge_prefix,
            username,
            content,
            color,
            source_lang,
            &source_lang.to_uppercase(),
        )
        .await;
    }

    /// Displays a message with translation, showing romanization and translated text.
    ///
    /// This is the core display function used by both auto-detected and forced translations.
    async fn display_translated_message(
        &self,
        badge_prefix: &str,
        username: &str,
        content: &str,
        color: &Option<String>,
        source_lang: &str,
        lang_display_name: &str,
    ) {
        let colored_name = colorize_username(username, color);

        // Display original message
        println!(
            "{}{}: {}",
            badge_prefix,
            colored_name,
            content.bright_white()
        );

        let lang_indicator = format!("[{}]", lang_display_name);

        // Only show spinner if we need to make an API call (not cached)
        let needs_api = self.translator.needs_api_call(content, source_lang);
        let stop_spinner = if needs_api {
            Some(start_spinner(&lang_indicator))
        } else {
            None
        };

        // Attempt translation (which now includes romanization)
        let result = self.translator.translate(content, source_lang).await;

        // Stop spinner and clear the line if we started one
        if let Some(stop_flag) = stop_spinner {
            stop_flag.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            clear_current_line();
        }

        match result {
            Ok(Some(translation_result)) => {
                // Only show romanization and translation if translation is different from original
                let has_translation = !translation_result
                    .translation
                    .trim()
                    .eq_ignore_ascii_case(content.trim());

                if has_translation {
                    // Show romanization (Google's or any_ascii fallback)
                    if let Some(romanization) =
                        get_romanization(content, translation_result.romanization.as_deref())
                    {
                        println!(
                            "  {} {}",
                            "♪".bright_yellow(),
                            romanization.bright_white().dimmed()
                        );
                    }

                    // Display translation with language indicator
                    let lang_indicator = lang_indicator.bright_magenta().bold();

                    println!(
                        "  {} {} {}",
                        "↳".bright_cyan(),
                        lang_indicator,
                        translation_result.translation.bright_green().italic()
                    );
                }
            }
            Ok(None) => {
                // Text was ASCII-only, no translation needed
            }
            Err(e) => {
                // Show error but don't crash
                debug!("Translation failed: {}", e);
                println!(
                    "  {} {}",
                    "Error:".bright_red().bold(),
                    "Message could not be translated.".bright_red()
                );
            }
        }
    }

    /// Handles a transcription event from the STT pipeline.
    ///
    /// Interim results are tracked silently (no terminal output) so we know
    /// speech is active. Final results are displayed with the `[Audio]` prefix
    /// and passed through the translation pipeline.
    async fn handle_transcription_event(&mut self, event: TranscriptionEvent) {
        match event {
            TranscriptionEvent::Interim { transcript } => {
                // Track the interim silently — no terminal output.
                // This keeps `current_interim` populated so the timeout
                // auto-commit can rescue segments Deepgram never finalizes.
                self.current_interim = Some(transcript);
                self.interim_set_at = Some(tokio::time::Instant::now());
            }
            TranscriptionEvent::Final {
                transcript,
                confidence,
            } => {
                // Deduplicate: skip if this final is identical to the last one.
                // Deepgram can send multiple is_final segments with overlapping text.
                let is_duplicate = self
                    .last_final_transcript
                    .as_ref()
                    .is_some_and(|last| last == &transcript);

                if is_duplicate {
                    self.clear_interim_transcript();
                } else {
                    self.clear_interim_transcript();
                    self.last_final_transcript = Some(transcript.clone());
                    self.display_final_transcript(&transcript, confidence).await;
                }
            }
            TranscriptionEvent::Error(msg) => {
                self.clear_interim_transcript();
                println!(
                    "{} {}",
                    "[Audio]"
                        .truecolor(255, 255, 255)
                        .on_truecolor(200, 30, 30)
                        .bold(),
                    msg.bright_red()
                );
            }
            TranscriptionEvent::Shutdown => {
                self.clear_interim_transcript();
                info!("Transcription pipeline shut down");
            }
        }
    }

    /// Resets interim tracking state without any terminal output.
    ///
    /// Called before printing final results or chat messages so stale
    /// interim data doesn't interfere with timeout auto-commit logic.
    fn clear_interim_transcript(&mut self) {
        self.current_interim = None;
        self.interim_set_at = None;
    }

    /// Displays a finalized transcript and optionally translates it.
    ///
    /// Shows the transcript with `[Audio]` prefix and a visual divider,
    /// then runs it through the translation pipeline if non-English.
    async fn display_final_transcript(&self, transcript: &str, _confidence: f64) {
        print_divider();

        println!(
            "{} {}",
            "[Audio]"
                .truecolor(255, 255, 255)
                .on_truecolor(138, 43, 226)
                .bold(),
            transcript.bright_white()
        );

        // Run through translation pipeline (same logic as chat messages)
        if let Some(ref default_lang) = self.config.default_language {
            self.display_stt_translation(transcript, default_lang).await;
        } else {
            let detection = self.translator.detect_language(transcript);
            if let Some(ref det) = detection {
                if !det.is_english && det.confidence >= self.config.detection_confidence_threshold {
                    self.display_stt_translation(transcript, &det.language)
                        .await;
                }
            }
        }
    }

    /// Translates an STT transcript and displays the romanization and translation.
    ///
    /// Shows a spinner while waiting for the translation API, matching the
    /// visual behavior of chat message translations.
    async fn display_stt_translation(&self, text: &str, source_lang: &str) {
        let lang_indicator = format!("[{}]", source_lang.to_uppercase());

        // Show spinner only when an API call is needed (not cached)
        let needs_api = self.translator.needs_api_call(text, source_lang);
        let stop_spinner = if needs_api {
            Some(start_spinner(&lang_indicator))
        } else {
            None
        };

        let result = self.translator.translate(text, source_lang).await;

        // Stop spinner and clear its line if we started one
        if let Some(stop_flag) = stop_spinner {
            stop_flag.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            clear_current_line();
        }

        match result {
            Ok(Some(result)) => {
                let has_translation = !result.translation.trim().eq_ignore_ascii_case(text.trim());

                if has_translation {
                    if let Some(romanization) =
                        get_romanization(text, result.romanization.as_deref())
                    {
                        println!(
                            "  {} {}",
                            "♪".bright_yellow(),
                            romanization.bright_white().dimmed()
                        );
                    }
                    println!(
                        "  {} {} {}",
                        "↳".bright_cyan(),
                        lang_indicator.bright_magenta().bold(),
                        result.translation.bright_green().italic()
                    );
                }
            }
            Ok(None) => {} // ASCII-only, no translation needed
            Err(e) => {
                debug!("STT translation failed: {}", e);
            }
        }
    }
}

/// Colorizes a username based on Twitch color or generates one.
fn colorize_username(username: &str, color: &Option<String>) -> colored::ColoredString {
    if let Some(hex) = color {
        if let Some((r, g, b)) = parse_hex_color(hex) {
            return username.truecolor(r, g, b).bold();
        }
    }

    // Generate a consistent color based on username hash
    let hash = username
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_add(b as u32));
    let colors = [
        (255, 0, 0),     // Red
        (0, 255, 0),     // Green
        (0, 0, 255),     // Blue
        (255, 255, 0),   // Yellow
        (255, 0, 255),   // Magenta
        (0, 255, 255),   // Cyan
        (255, 127, 0),   // Orange
        (127, 255, 0),   // Lime
        (255, 0, 127),   // Pink
        (0, 127, 255),   // Sky Blue
        (127, 0, 255),   // Purple
        (255, 127, 127), // Light Red
    ];

    let (r, g, b) = colors[(hash as usize) % colors.len()];
    username.truecolor(r, g, b).bold()
}

/// Parses a hex color string (e.g., "#FF0000") into RGB components.
fn parse_hex_color(hex: &str) -> Option<(u8, u8, u8)> {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }

    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;

    Some((r, g, b))
}

/// Builds a badge prefix string with icons for broadcaster, mod, and VIP.
///
/// Uses bracketed text badges similar to Twitch's style.
fn build_badge_prefix(is_broadcaster: bool, is_mod: bool, is_vip: bool) -> String {
    let mut prefix = String::new();

    if is_broadcaster {
        // Broadcaster: red [B] badge
        prefix.push_str(&format!(
            "{} ",
            "[B]"
                .truecolor(255, 255, 255)
                .on_truecolor(255, 0, 0)
                .bold()
        ));
    } else if is_mod {
        // Moderator: green [M] badge (like Twitch's green sword)
        prefix.push_str(&format!(
            "{} ",
            "[M]"
                .truecolor(255, 255, 255)
                .on_truecolor(0, 173, 3)
                .bold()
        ));
    } else if is_vip {
        // VIP: pink [V] badge
        prefix.push_str(&format!(
            "{} ",
            "[V]"
                .truecolor(255, 255, 255)
                .on_truecolor(224, 5, 185)
                .bold()
        ));
    }

    prefix
}

/// Gets the current terminal width, with a fallback default.
fn get_terminal_width() -> usize {
    terminal_size()
        .map(|(Width(w), _)| w as usize)
        .unwrap_or(80)
}

/// Prints a divider line that spans the terminal width.
fn print_divider() {
    let width = get_terminal_width();
    let divider: String = "─".repeat(width.saturating_sub(1));
    println!("{}", divider.bright_black());
}

/// Prints a thick divider line that spans the terminal width.
fn print_thick_divider() {
    let width = get_terminal_width();
    let divider: String = "━".repeat(width.saturating_sub(1));
    println!("{}", divider.bright_black());
}

/// Gets romanization, using any_ascii as fallback if Google doesn't provide one.
///
/// Returns None if the text is ASCII-only or if transliteration matches the original.
fn get_romanization(text: &str, google_romanization: Option<&str>) -> Option<String> {
    // Use Google's romanization if available
    if let Some(rom) = google_romanization {
        if !rom.is_empty() {
            return Some(rom.to_string());
        }
    }

    // Fallback to any_ascii for non-ASCII text
    if !text.is_ascii() {
        let transliterated = any_ascii(text);
        // Only return if meaningfully different from original
        if !transliterated.is_empty() && transliterated != text {
            return Some(transliterated);
        }
    }

    None
}

/// Spinner frames for the loading animation.
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Starts a spinner animation in a separate task.
/// Returns an Arc<AtomicBool> that can be set to true to stop the spinner.
fn start_spinner(lang_indicator: &str) -> Arc<AtomicBool> {
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_flag_clone = stop_flag.clone();
    let lang = lang_indicator.to_string();

    // Print first frame synchronously with newline
    println!(
        "  {} {} {}",
        "↳".bright_cyan(),
        lang_indicator.bright_magenta().bold(),
        format!("{} Translating...", SPINNER_FRAMES[0]).bright_yellow()
    );

    tokio::spawn(async move {
        let mut frame_idx = 1;
        while !stop_flag_clone.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(80)).await;
            if stop_flag_clone.load(Ordering::SeqCst) {
                break;
            }
            let frame = SPINNER_FRAMES[frame_idx % SPINNER_FRAMES.len()];
            // Move up to spinner line, clear, print next frame
            let _ = execute!(
                stdout(),
                cursor::MoveUp(1),
                cursor::MoveToColumn(0),
                terminal::Clear(terminal::ClearType::CurrentLine)
            );
            println!(
                "  {} {} {}",
                "↳".bright_cyan(),
                lang.bright_magenta().bold(),
                format!("{} Translating...", frame).bright_yellow()
            );
            frame_idx += 1;
        }
    });

    stop_flag
}

/// Clears the spinner line and the empty line below, leaving cursor ready for next output.
fn clear_current_line() {
    // Cursor is on line below spinner. Clear current line first, then move up and clear spinner.
    let _ = execute!(
        stdout(),
        cursor::MoveToColumn(0),
        terminal::Clear(terminal::ClearType::CurrentLine),
        cursor::MoveUp(1),
        cursor::MoveToColumn(0),
        terminal::Clear(terminal::ClearType::CurrentLine)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hex_color_valid() {
        assert_eq!(parse_hex_color("#FF0000"), Some((255, 0, 0)));
        assert_eq!(parse_hex_color("#00FF00"), Some((0, 255, 0)));
        assert_eq!(parse_hex_color("#0000FF"), Some((0, 0, 255)));
        assert_eq!(parse_hex_color("#FFFFFF"), Some((255, 255, 255)));
        assert_eq!(parse_hex_color("#000000"), Some((0, 0, 0)));
        assert_eq!(parse_hex_color("#ff5500"), Some((255, 85, 0)));
    }

    #[test]
    fn test_parse_hex_color_without_hash() {
        assert_eq!(parse_hex_color("FF0000"), Some((255, 0, 0)));
    }

    #[test]
    fn test_parse_hex_color_invalid() {
        assert_eq!(parse_hex_color("#FFF"), None); // Too short
        assert_eq!(parse_hex_color("#FFFFFFF"), None); // Too long
        assert_eq!(parse_hex_color(""), None); // Empty
        assert_eq!(parse_hex_color("#GGGGGG"), None); // Invalid hex
    }

    #[test]
    fn test_build_badge_prefix_broadcaster() {
        let prefix = build_badge_prefix(true, false, false);
        assert!(prefix.contains("[B]"));
    }

    #[test]
    fn test_build_badge_prefix_moderator() {
        let prefix = build_badge_prefix(false, true, false);
        assert!(prefix.contains("[M]"));
    }

    #[test]
    fn test_build_badge_prefix_vip() {
        let prefix = build_badge_prefix(false, false, true);
        assert!(prefix.contains("[V]"));
    }

    #[test]
    fn test_build_badge_prefix_no_badge() {
        let prefix = build_badge_prefix(false, false, false);
        assert!(prefix.is_empty());
    }

    #[test]
    fn test_build_badge_prefix_broadcaster_takes_precedence() {
        // Broadcaster should take precedence over mod and VIP
        let prefix = build_badge_prefix(true, true, true);
        assert!(prefix.contains("[B]"));
        assert!(!prefix.contains("[M]"));
        assert!(!prefix.contains("[V]"));
    }

    #[test]
    fn test_get_romanization_with_google() {
        // Google romanization takes precedence
        let result = get_romanization("привет", Some("privet"));
        assert_eq!(result, Some("privet".to_string()));
    }

    #[test]
    fn test_get_romanization_fallback_to_any_ascii() {
        // Falls back to any_ascii when Google doesn't provide romanization
        let result = get_romanization("привет", None);
        assert!(result.is_some());
        assert_eq!(result, Some("privet".to_string()));
    }

    #[test]
    fn test_get_romanization_empty_google() {
        // Empty Google romanization should trigger fallback
        let result = get_romanization("привет", Some(""));
        assert!(result.is_some());
    }

    #[test]
    fn test_get_romanization_ascii_text() {
        // ASCII text shouldn't get romanization
        let result = get_romanization("hello", None);
        assert_eq!(result, None);
    }

    #[test]
    fn test_colorize_username_with_color() {
        let colored = colorize_username("TestUser", &Some("#FF0000".to_string()));
        // Just verify it doesn't panic; actual color is hard to test
        assert!(!colored.to_string().is_empty());
    }

    #[test]
    fn test_colorize_username_without_color() {
        let colored = colorize_username("TestUser", &None);
        // Should generate a consistent color based on username hash
        assert!(!colored.to_string().is_empty());
    }

    #[test]
    fn test_colorize_username_consistency() {
        // Same username should produce same color
        let colored1 = colorize_username("TestUser", &None);
        let colored2 = colorize_username("TestUser", &None);
        assert_eq!(colored1.to_string(), colored2.to_string());
    }
}
