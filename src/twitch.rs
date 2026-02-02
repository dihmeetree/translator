//! Twitch IRC client module.
//!
//! Handles connecting to Twitch chat via IRC over WebSocket and processing messages.

use crate::config::Config;
use crate::events::{
    self, ChannelChangedEvent, ChatMessageEvent, ReplyContext, TranscriptionMessageEvent, WebEvent,
};
use crate::transcription::{TranscriptionConfig, TranscriptionEvent};
use crate::translator::{TranslationResult, Translator};
use crate::web::ControlCommand;
use any_ascii::any_ascii;
use anyhow::{Context, Result};
use colored::Colorize;
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use terminal_size::{terminal_size, Width};
use tokio::sync::{broadcast, mpsc, watch};
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

    /// Broadcast sender for WebSocket event distribution.
    /// Each outgoing event is sent to all connected web UI clients.
    /// If no subscribers exist, sends are silently dropped.
    event_tx: broadcast::Sender<WebEvent>,

    /// Generation counter incremented on each channel switch.
    /// Spawned tasks capture the current value and skip broadcasting
    /// if the generation has changed, preventing stale events from
    /// the old channel appearing after a switch.
    generation: Arc<AtomicU64>,

    /// Receiver for control commands from the web UI (e.g., channel switching).
    control_rx: mpsc::Receiver<ControlCommand>,

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
    /// A control command was received from the web UI (e.g., switch channel).
    SwitchChannel(String),
}

impl TwitchClient {
    /// Creates a new TwitchClient and connects to the IRC server.
    ///
    /// # Arguments
    ///
    /// * `config` - Application configuration including channel and credentials.
    /// * `translator` - Translation service for non-English messages.
    /// * `event_tx` - Broadcast sender for distributing events to web UI clients.
    /// * `control_rx` - Receiver for control commands from the web UI.
    pub async fn new(
        config: Config,
        translator: Translator,
        event_tx: broadcast::Sender<WebEvent>,
        control_rx: mpsc::Receiver<ControlCommand>,
    ) -> Result<Self> {
        info!("Connecting to Twitch IRC...");

        let (ws_stream, _) = connect_async(TWITCH_IRC_WSS)
            .await
            .context("Failed to connect to Twitch IRC WebSocket")?;

        let mut client = Self {
            config: config.clone(),
            translator,
            ws_stream,
            event_tx,
            generation: Arc::new(AtomicU64::new(0)),
            control_rx,
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

        let irc_timeout = Duration::from_secs(self.config.irc_timeout_secs);

        loop {
            // Produce a LoopAction from select! without holding &mut self borrows,
            // then handle it separately to allow full &mut self access.
            // Compute the interim commit deadline before entering select!
            // so we don't borrow self inside the macro.
            let interim_deadline = self.interim_set_at.map(|t| t + INTERIM_COMMIT_TIMEOUT);

            let action = if let Some(ref mut stt_rx) = self.stt_rx {
                tokio::select! {
                    ws_result = timeout(irc_timeout, self.ws_stream.next()) => {
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
                    cmd = self.control_rx.recv() => {
                        match cmd {
                            Some(ControlCommand::SwitchChannel(ch)) => LoopAction::SwitchChannel(ch),
                            None => continue,
                        }
                    }
                }
            } else {
                tokio::select! {
                    ws_result = timeout(irc_timeout, self.ws_stream.next()) => {
                        match ws_result {
                            Ok(Some(Ok(Message::Text(text)))) => LoopAction::IrcMessage(text),
                            Ok(Some(Ok(_))) => continue,
                            Ok(Some(Err(e))) => LoopAction::IrcError(e.to_string()),
                            Ok(None) => LoopAction::IrcClosed,
                            Err(_) => LoopAction::IrcTimeout,
                        }
                    }
                    cmd = self.control_rx.recv() => {
                        match cmd {
                            Some(ControlCommand::SwitchChannel(ch)) => LoopAction::SwitchChannel(ch),
                            None => continue,
                        }
                    }
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
                    self.handle_transcription_event(event);
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
                        self.display_final_transcript(&transcript, 0.0);
                    }
                }
                LoopAction::SwitchChannel(new_channel) => {
                    self.handle_channel_switch(&new_channel).await?;
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
            self.process_chat_message(chat_msg);
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

    /// Processes a chat message by spawning translation as a background task.
    ///
    /// The message is immediately dispatched to a spawned task that handles
    /// language detection, translation, terminal display, and WebSocket broadcast.
    /// This keeps the main event loop free to process the next IRC or STT event
    /// without waiting for the translation API.
    fn process_chat_message(&mut self, msg: ChatMessage) {
        // Reset interim state so a stale timeout doesn't fire during chat output
        self.clear_interim_transcript();

        let translator = self.translator.clone();
        let event_tx = self.event_tx.clone();
        let generation = Arc::clone(&self.generation);
        let gen_at_spawn = generation.load(Ordering::Relaxed);
        let config_default_lang = self.config.default_language.clone();
        let config_threshold = self.config.detection_confidence_threshold;

        tokio::spawn(async move {
            let display = msg
                .display_name
                .as_ref()
                .unwrap_or(&msg.username)
                .to_string();

            let badge_prefix = build_badge_prefix(msg.is_broadcaster, msg.is_mod, msg.is_vip);

            // Print divider before each message
            print_divider();

            // Display reply context if this is a reply
            if let Some(ref reply) = msg.reply_to {
                display_reply_context(reply);
            }

            // Detect language and translate if non-English
            let (source_lang, lang_name, translation_result) = detect_and_translate(
                &translator,
                &msg.content,
                &config_default_lang,
                config_threshold,
            )
            .await;

            // Terminal display
            let colored_name = colorize_username(&display, &msg.color);
            if let Some(ref result) = translation_result {
                println!(
                    "{}{}: {}",
                    badge_prefix,
                    colored_name,
                    msg.content.bright_white()
                );
                display_translation_result(&msg.content, lang_name.as_deref(), result);
            } else {
                println!("{}{}: {}", badge_prefix, colored_name, msg.content);
            }

            // Skip broadcasting if the channel has changed since this task spawned
            if generation.load(Ordering::Relaxed) != gen_at_spawn {
                return;
            }

            // Broadcast to WebSocket clients
            let event = ChatMessageEvent {
                username: display,
                color: msg.color.clone(),
                is_broadcaster: msg.is_broadcaster,
                is_mod: msg.is_mod,
                is_vip: msg.is_vip,
                content: msg.content.clone(),
                translation: translation_result.as_ref().map(|r| r.translation.clone()),
                romanization: translation_result
                    .as_ref()
                    .and_then(|r| r.romanization.clone()),
                source_language: source_lang,
                language_name: lang_name,
                reply_to: msg.reply_to.as_ref().map(|r| ReplyContext {
                    parent_username: r.parent_display_name.clone(),
                    parent_message: r.parent_msg_body.clone(),
                }),
                timestamp: events::current_timestamp_ms(),
            };
            let _ = event_tx.send(WebEvent::ChatMessage(event));
        });
    }

    /// Handles a transcription event from the STT pipeline.
    ///
    /// Interim results are tracked silently (no terminal output) so we know
    /// speech is active. Final results are displayed with the `[Audio]` prefix
    /// and passed through the translation pipeline.
    fn handle_transcription_event(&mut self, event: TranscriptionEvent) {
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
                    self.display_final_transcript(&transcript, confidence);
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

    /// Switches to a new Twitch channel at runtime.
    ///
    /// Parts the old IRC channel, joins the new one, updates config,
    /// resets transcript state, and restarts the transcription pipeline
    /// if it was active.
    async fn handle_channel_switch(&mut self, new_channel: &str) -> Result<()> {
        let old_channel = self.config.channel.clone();
        info!("Switching channel: #{} -> #{}", old_channel, new_channel);

        // Bump generation so in-flight spawned tasks from the old channel
        // skip broadcasting their results.
        self.generation.fetch_add(1, Ordering::Relaxed);

        // PART the old IRC channel
        self.send_raw(&format!("PART #{}", old_channel)).await?;

        // Update the channel in config
        self.config.channel = new_channel.to_string();

        // JOIN the new IRC channel
        self.send_raw(&format!("JOIN #{}", self.config.channel))
            .await?;

        // Reset transcript state
        self.clear_interim_transcript();
        self.last_final_transcript = None;

        // Restart the transcription pipeline if it was active
        if self.stt_shutdown_tx.is_some() {
            self.restart_transcription_pipeline();
        }

        // Broadcast ChannelChanged to WebSocket clients now that the switch is done
        let event = WebEvent::ChannelChanged(ChannelChangedEvent {
            channel: self.config.channel.clone(),
            timestamp: events::current_timestamp_ms(),
        });
        let _ = self.event_tx.send(event);

        info!("Now listening on #{}", self.config.channel);
        print_thick_divider();
        println!(
            "{}",
            format!(
                "Switched to #{} - waiting for messages...",
                self.config.channel
            )
            .bright_green()
            .bold()
        );

        Ok(())
    }

    /// Shuts down the current transcription pipeline and starts a new one
    /// for the current channel.
    ///
    /// Signals the old pipeline to shut down via the watch channel, then
    /// spawns a fresh pipeline with the updated channel name.
    fn restart_transcription_pipeline(&mut self) {
        // Signal the old pipeline to shut down
        if let Some(ref shutdown_tx) = self.stt_shutdown_tx {
            let _ = shutdown_tx.send(true);
        }
        self.stt_shutdown_tx = None;
        self.stt_rx = None;

        // Start a new pipeline for the new channel
        if let Some(ref api_key) = self.config.deepgram_api_key {
            let (event_tx, event_rx) = mpsc::channel::<TranscriptionEvent>(64);
            let (shutdown_tx, shutdown_rx) = watch::channel(false);

            let stt_config = TranscriptionConfig {
                api_key: api_key.clone(),
                channel: self.config.channel.clone(),
                language: self.config.stt_language.clone(),
                model: self.config.deepgram_model.clone(),
            };

            tokio::spawn(crate::transcription::run_pipeline(
                stt_config,
                event_tx,
                shutdown_rx,
            ));

            self.stt_rx = Some(event_rx);
            self.stt_shutdown_tx = Some(shutdown_tx);

            info!(
                "Restarted transcription pipeline for #{}",
                self.config.channel
            );
        }
    }

    /// Spawns a background task to display and broadcast a finalized transcript.
    ///
    /// Shows the transcript with `[Audio]` prefix and a visual divider,
    /// then runs it through the translation pipeline if non-English.
    /// Also broadcasts a [`WebEvent::Transcription`] to WebSocket clients.
    ///
    /// Runs in a spawned task so the main event loop is not blocked by
    /// translation API calls, allowing chat and audio to process in parallel.
    fn display_final_transcript(&self, transcript: &str, _confidence: f64) {
        let translator = self.translator.clone();
        let event_tx = self.event_tx.clone();
        let generation = Arc::clone(&self.generation);
        let gen_at_spawn = generation.load(Ordering::Relaxed);
        let config_default_lang = self.config.default_language.clone();
        let config_threshold = self.config.detection_confidence_threshold;
        let transcript = transcript.to_string();

        tokio::spawn(async move {
            print_divider();

            println!(
                "{} {}",
                "[Audio]"
                    .truecolor(255, 255, 255)
                    .on_truecolor(138, 43, 226)
                    .bold(),
                transcript.bright_white()
            );

            // Detect language and translate if non-English
            let (source_lang, lang_name, translation_result) = detect_and_translate(
                &translator,
                &transcript,
                &config_default_lang,
                config_threshold,
            )
            .await;

            // Terminal display of translation
            if let Some(ref result) = translation_result {
                display_translation_result(&transcript, lang_name.as_deref(), result);
            }

            // Skip broadcasting if the channel has changed since this task spawned
            if generation.load(Ordering::Relaxed) != gen_at_spawn {
                return;
            }

            // Broadcast to WebSocket clients
            let event = TranscriptionMessageEvent {
                transcript: transcript.clone(),
                translation: translation_result.as_ref().map(|r| r.translation.clone()),
                romanization: translation_result
                    .as_ref()
                    .and_then(|r| r.romanization.clone()),
                source_language: source_lang,
                language_name: lang_name,
                timestamp: events::current_timestamp_ms(),
            };
            let _ = event_tx.send(WebEvent::Transcription(event));
        });
    }
}

/// Detects the language of the given text and translates it if non-English.
///
/// Handles both "forced language" mode (when `default_lang` is set) and
/// auto-detection mode. Returns a tuple of `(source_lang, lang_name, translation_result)`.
async fn detect_and_translate(
    translator: &Translator,
    text: &str,
    default_lang: &Option<String>,
    confidence_threshold: f64,
) -> (Option<String>, Option<String>, Option<TranslationResult>) {
    if let Some(ref lang) = *default_lang {
        let result = translate_for_display(translator, text, lang).await;
        (Some(lang.clone()), Some(lang.to_uppercase()), result)
    } else {
        let detection = translator.detect_language(text);
        match detection {
            Some(ref det) if !det.is_english && det.confidence >= confidence_threshold => {
                let result = translate_for_display(translator, text, &det.language).await;
                (
                    Some(det.language.clone()),
                    Some(det.language_name.clone()),
                    result,
                )
            }
            _ => (None, None, None),
        }
    }
}

/// Prints the translation result (romanization + translated text) to the terminal.
///
/// Shows nothing if the translation is identical to the source text (case-insensitive).
fn display_translation_result(
    source_text: &str,
    lang_name: Option<&str>,
    result: &TranslationResult,
) {
    let has_translation = !result
        .translation
        .trim()
        .eq_ignore_ascii_case(source_text.trim());
    if has_translation {
        if let Some(romanization) = get_romanization(source_text, result.romanization.as_deref()) {
            println!(
                "  {} {}",
                "♪".bright_yellow(),
                romanization.bright_white().dimmed()
            );
        }
        let lang_indicator = format!("[{}]", lang_name.unwrap_or("?"))
            .bright_magenta()
            .bold();
        println!(
            "  {} {} {}",
            "↳".bright_cyan(),
            lang_indicator,
            result.translation.bright_green().italic()
        );
    }
}

/// Displays the context of a reply (the message being replied to).
///
/// Prints a dimmed "replying to @User: message" line above the chat message.
fn display_reply_context(reply: &ReplyInfo) {
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

/// Translates text using the translator, returning the result without terminal spinner.
///
/// Used by spawned background tasks for parallel translation processing.
/// Logs errors but does not print to terminal on failure (the spawned task
/// handles its own terminal output).
async fn translate_for_display(
    translator: &Translator,
    content: &str,
    source_lang: &str,
) -> Option<TranslationResult> {
    match translator.translate(content, source_lang).await {
        Ok(Some(tr)) => Some(tr),
        Ok(None) => None,
        Err(e) => {
            debug!("Translation failed: {}", e);
            None
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
