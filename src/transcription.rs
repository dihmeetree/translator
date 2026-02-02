//! Real-time audio transcription module for Twitch streams.
//!
//! Captures audio from a live Twitch stream using streamlink + ffmpeg,
//! then streams it to Deepgram's speech-to-text WebSocket API for
//! low-latency transcription. Results are delivered via an async channel
//! to the main display loop.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

/// Size of audio chunks to read from ffmpeg stdout, in bytes.
/// 16kHz sample rate * 1 channel * 2 bytes/sample * 0.25 seconds = 8000 bytes per 250ms.
const AUDIO_CHUNK_SIZE: usize = 8000;

/// Maximum reconnect backoff duration.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Initial reconnect backoff duration.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// Minimum transcript length to display (filters noise artifacts).
const MIN_TRANSCRIPT_LEN: usize = 2;

/// Events sent from the transcription pipeline to the display loop.
#[derive(Debug, Clone)]
pub enum TranscriptionEvent {
    /// A partial (interim) transcription result that may be refined.
    Interim {
        /// The interim transcript text.
        transcript: String,
    },
    /// A finalized transcription result that will not change.
    Final {
        /// The final transcript text.
        transcript: String,
        /// Deepgram's confidence score for this result.
        confidence: f64,
    },
    /// A recoverable error occurred in the pipeline.
    Error(String),
    /// The pipeline has fully shut down.
    Shutdown,
}

/// Configuration for the transcription pipeline.
#[derive(Debug, Clone)]
pub struct TranscriptionConfig {
    /// Deepgram API key.
    pub api_key: String,
    /// Twitch channel name (without #).
    pub channel: String,
    /// Deepgram language code (e.g., "ru", "en", "multi").
    pub language: String,
    /// Deepgram model (e.g., "nova-2").
    pub model: String,
}

/// Top-level Deepgram streaming API response.
#[derive(Debug, Deserialize)]
struct DeepgramResponse {
    /// Response type. We only process "Results" messages.
    #[serde(rename = "type")]
    response_type: Option<String>,
    /// Channel data containing transcription alternatives.
    channel: Option<DeepgramChannel>,
    /// Whether this audio segment's transcript is finalized.
    is_final: Option<bool>,
}

/// Channel data from a Deepgram response.
#[derive(Debug, Deserialize)]
struct DeepgramChannel {
    /// List of transcription alternatives, ordered by confidence.
    alternatives: Vec<DeepgramAlternative>,
}

/// A single transcription alternative from Deepgram.
#[derive(Debug, Deserialize)]
struct DeepgramAlternative {
    /// The transcribed text.
    transcript: String,
    /// Confidence score between 0.0 and 1.0.
    confidence: f64,
}

/// Type alias for the Deepgram WebSocket stream.
type DgWsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Verifies that a required external command is installed and accessible on PATH.
///
/// # Errors
///
/// Returns an error with the install hint if the command is not found.
fn check_dependency(command: &str, install_hint: &str) -> Result<()> {
    match std::process::Command::new("which")
        .arg(command)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => Ok(()),
        _ => anyhow::bail!("'{}' not found on PATH. {}", command, install_hint),
    }
}

/// Spawns the streamlink process to extract the HLS stream from Twitch.
///
/// Returns a child process whose stdout emits the raw transport stream data.
/// The `--twitch-low-latency` flag reduces buffer for lower transcription latency.
/// Stderr is piped so we can log errors (e.g., "No playable streams found").
fn spawn_streamlink(channel: &str) -> Result<tokio::process::Child> {
    info!("Starting streamlink for channel: {}", channel);

    tokio::process::Command::new("streamlink")
        .arg(format!("twitch.tv/{}", channel))
        // Use audio_only if available (smaller segments, faster startup),
        // fall back to worst quality which still contains audio but has
        // the smallest video segments to download and demux.
        .arg("audio_only,worst")
        .arg("--stdout")
        .arg("--twitch-low-latency")
        // Start from the most recent HLS segment instead of buffering
        // multiple segments, reducing startup latency.
        .args(["--hls-live-edge", "1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("Failed to spawn streamlink process")
}

/// Spawns the ffmpeg process to convert the transport stream to raw PCM audio.
///
/// Takes streamlink's stdout as its stdin and outputs 16kHz mono 16-bit signed
/// little-endian PCM on stdout, which Deepgram accepts directly.
fn spawn_ffmpeg(streamlink_stdout: tokio::process::ChildStdout) -> Result<tokio::process::Child> {
    info!("Starting ffmpeg audio decoder");

    // Convert tokio ChildStdout to std Stdio for piping to ffmpeg's stdin.
    let stdin_stdio: Stdio = streamlink_stdout
        .try_into()
        .context("Failed to convert streamlink stdout for ffmpeg stdin")?;

    tokio::process::Command::new("ffmpeg")
        // Reduce ffmpeg's internal buffering for lower startup latency.
        // probesize must be large enough for MPEG-TS format detection
        // (32K was too small and caused ffmpeg to stall indefinitely).
        .args(["-probesize", "1000000"])
        .args(["-analyzeduration", "500000"])
        .args(["-fflags", "+nobuffer"])
        .args(["-flags", "low_delay"])
        .args(["-i", "pipe:0"])
        .args(["-vn"]) // Discard video entirely — only decode audio
        .args(["-f", "s16le"])
        .args(["-acodec", "pcm_s16le"])
        .args(["-ar", "16000"])
        .args(["-ac", "1"])
        .args(["-loglevel", "quiet"])
        .arg("pipe:1")
        .stdin(stdin_stdio)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("Failed to spawn ffmpeg process")
}

/// Reads and logs streamlink's stderr output line by line.
///
/// Runs as a background task so errors like "No playable streams found"
/// are visible in the log instead of being silently discarded.
async fn drain_streamlink_stderr(stderr: tokio::process::ChildStderr) {
    use tokio::io::AsyncBufReadExt;
    let reader = tokio::io::BufReader::new(stderr);
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if !line.is_empty() {
            debug!("streamlink: {}", line);
        }
    }
}

/// Connects to Deepgram's streaming speech-to-text WebSocket API.
///
/// Builds a custom HTTP request with all required WebSocket handshake headers
/// and the Deepgram authorization token, then establishes the connection.
async fn connect_deepgram(config: &TranscriptionConfig) -> Result<DgWsStream> {
    let url = format!(
        "wss://api.deepgram.com/v1/listen?\
         encoding=linear16&sample_rate=16000&channels=1\
         &model={}&interim_results=true&smart_format=true&language={}",
        config.model, config.language
    );

    info!(
        "Connecting to Deepgram: model={}, language={}",
        config.model, config.language
    );

    // tokio-tungstenite requires all WebSocket handshake headers when using
    // a raw http::Request. We include Host, Connection, Upgrade,
    // Sec-WebSocket-Version, Sec-WebSocket-Key, plus Authorization.
    use tokio_tungstenite::tungstenite::handshake::client::generate_key;
    use tokio_tungstenite::tungstenite::http;

    let request = http::Request::builder()
        .uri(&url)
        .header("Host", "api.deepgram.com")
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", generate_key())
        .header("Authorization", format!("Token {}", config.api_key))
        .body(())
        .context("Failed to build Deepgram WebSocket request")?;

    let (ws_stream, response) = connect_async(request)
        .await
        .context("Failed to connect to Deepgram WebSocket API")?;

    info!("Connected to Deepgram (HTTP {})", response.status());

    Ok(ws_stream)
}

/// Parses a Deepgram JSON response text frame into a `TranscriptionEvent`.
///
/// Returns `None` for non-Results messages, empty transcripts, or very short
/// utterances that are likely noise artifacts.
fn parse_deepgram_response(json_text: &str) -> Option<TranscriptionEvent> {
    let response: DeepgramResponse = match serde_json::from_str(json_text) {
        Ok(r) => r,
        Err(e) => {
            debug!("Failed to parse Deepgram response: {}", e);
            return None;
        }
    };

    // Only process "Results" type messages
    match response.response_type.as_deref() {
        Some("Results") => {}
        _ => return None,
    }

    let channel = response.channel.as_ref()?;
    let alt = channel.alternatives.first()?;
    let transcript = alt.transcript.trim().to_string();

    // Filter out empty or very short transcripts (noise artifacts)
    if transcript.len() < MIN_TRANSCRIPT_LEN {
        return None;
    }

    if response.is_final.unwrap_or(false) {
        Some(TranscriptionEvent::Final {
            transcript,
            confidence: alt.confidence,
        })
    } else {
        Some(TranscriptionEvent::Interim { transcript })
    }
}

/// Starts the transcription pipeline and runs it until shutdown.
///
/// This function is designed to be called via `tokio::spawn`. It manages
/// the full lifecycle: dependency checking, process spawning, WebSocket
/// connection, audio streaming, and reconnection with exponential backoff.
///
/// # Arguments
///
/// * `config` - Transcription configuration (API key, channel, language, model).
/// * `event_tx` - Channel sender for delivering transcription events to the display loop.
/// * `shutdown_rx` - Watch receiver that signals when the app is shutting down.
pub async fn run_pipeline(
    config: TranscriptionConfig,
    event_tx: mpsc::Sender<TranscriptionEvent>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    // Pre-flight dependency checks
    if let Err(e) = check_dependency("streamlink", "Install with: pip install streamlink") {
        error!("{}", e);
        let _ = event_tx
            .send(TranscriptionEvent::Error(e.to_string()))
            .await;
        let _ = event_tx.send(TranscriptionEvent::Shutdown).await;
        return;
    }
    if let Err(e) = check_dependency("ffmpeg", "Install from: https://ffmpeg.org/download.html") {
        error!("{}", e);
        let _ = event_tx
            .send(TranscriptionEvent::Error(e.to_string()))
            .await;
        let _ = event_tx.send(TranscriptionEvent::Shutdown).await;
        return;
    }

    let mut backoff = INITIAL_BACKOFF;

    loop {
        // Check shutdown before each attempt
        if *shutdown_rx.borrow() {
            break;
        }

        match run_pipeline_once(&config, &event_tx, &mut shutdown_rx).await {
            Ok(()) => {
                // Stream ended (e.g., went offline or streamer switched scenes).
                // Reconnect after a short pause so we pick the stream back up
                // once it returns. Only an explicit shutdown signal should exit.
                // Reset backoff since the pipeline connected and ran successfully.
                backoff = INITIAL_BACKOFF;
                info!(
                    "Transcription pipeline ended cleanly, reconnecting in {:?}...",
                    backoff
                );

                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = shutdown_rx.changed() => { break; }
                }
            }
            Err(e) => {
                warn!(
                    "Transcription pipeline error: {}. Reconnecting in {:?}...",
                    e, backoff
                );
                let _ = event_tx
                    .send(TranscriptionEvent::Error(format!(
                        "STT error: {}. Retrying in {}s...",
                        e,
                        backoff.as_secs()
                    )))
                    .await;

                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = shutdown_rx.changed() => { break; }
                }

                // Exponential backoff with cap
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }

    let _ = event_tx.send(TranscriptionEvent::Shutdown).await;
    info!("Transcription pipeline shut down");
}

/// Single run of the pipeline: spawns processes, connects to Deepgram,
/// and streams audio until an error or clean shutdown occurs.
///
/// Returns `Ok(())` on clean finish (e.g., stream went offline),
/// or `Err` on a failure that should trigger reconnection.
async fn run_pipeline_once(
    config: &TranscriptionConfig,
    event_tx: &mpsc::Sender<TranscriptionEvent>,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<()> {
    // 1. Spawn streamlink
    let mut streamlink = spawn_streamlink(&config.channel)?;
    let streamlink_stdout = streamlink
        .stdout
        .take()
        .context("streamlink stdout not captured")?;

    // Drain streamlink stderr in the background so we can log errors
    // (e.g., "No playable streams found on this URL").
    if let Some(stderr) = streamlink.stderr.take() {
        tokio::spawn(drain_streamlink_stderr(stderr));
    }

    // 2. Spawn ffmpeg (takes ownership of streamlink_stdout via Stdio conversion)
    let mut ffmpeg = spawn_ffmpeg(streamlink_stdout)?;
    let mut ffmpeg_stdout = ffmpeg.stdout.take().context("ffmpeg stdout not captured")?;

    // 3. Wait for ffmpeg to produce a full audio chunk before connecting
    //    to Deepgram. Streamlink blocks output during Twitch pre-roll ads
    //    (often 15-30s), so connecting to Deepgram earlier would just cause
    //    it to time out from receiving no audio data.
    info!("Waiting for audio stream to start (may be delayed by Twitch ads)...");
    let mut first_chunk = vec![0u8; AUDIO_CHUNK_SIZE];
    let first_read = tokio::time::timeout(
        Duration::from_secs(90),
        ffmpeg_stdout.read(&mut first_chunk),
    )
    .await
    .context("Timed out waiting for audio from ffmpeg (stream may be offline)")?
    .context("Failed to read audio from ffmpeg")?;

    if first_read == 0 {
        anyhow::bail!("ffmpeg produced no audio (stream may be offline or unavailable)");
    }
    info!("Audio stream active ({} bytes buffered)", first_read);

    // 4. Now connect to Deepgram — audio is confirmed flowing.
    let ws_stream = connect_deepgram(config).await?;
    let (ws_sink, ws_stream_rx) = ws_stream.split();

    info!("Transcription pipeline running");

    // 5. Run audio sender and result receiver concurrently.
    //    Pass the first buffered chunk so it's sent before entering the read loop.
    let mut shutdown_a = shutdown_rx.clone();
    let mut shutdown_b = shutdown_rx.clone();
    let event_tx_clone = event_tx.clone();
    let first_chunk_data = first_chunk[..first_read].to_vec();

    let mut audio_task = tokio::spawn(async move {
        send_audio_to_deepgram(ffmpeg_stdout, ws_sink, &mut shutdown_a, first_chunk_data).await
    });

    let mut recv_task = tokio::spawn(async move {
        receive_deepgram_results(ws_stream_rx, event_tx_clone, &mut shutdown_b).await
    });

    // Wait for either task to finish or shutdown signal
    tokio::select! {
        result = &mut audio_task => {
            match result {
                Ok(Ok(())) => debug!("Audio sender finished"),
                Ok(Err(e)) => warn!("Audio sender error: {}", e),
                Err(e) => warn!("Audio sender task panicked: {}", e),
            }
        }
        result = &mut recv_task => {
            match result {
                Ok(Ok(())) => debug!("Result receiver finished"),
                Ok(Err(e)) => warn!("Result receiver error: {}", e),
                Err(e) => warn!("Result receiver task panicked: {}", e),
            }
        }
        _ = shutdown_rx.changed() => {
            info!("Shutdown signal received, stopping transcription");
        }
    }

    // Cleanup: abort any still-running spawned tasks so they release
    // their held resources (ffmpeg stdout, Deepgram WebSocket) before
    // we kill the child processes. Without this, the old tasks leak
    // across reconnect attempts and hold stale file descriptors.
    audio_task.abort();
    recv_task.abort();

    // Kill child processes and wait for them to fully exit so no
    // zombie processes linger when the pipeline reconnects.
    let _ = streamlink.kill().await;
    let _ = ffmpeg.kill().await;
    let _ = streamlink.wait().await;
    let _ = ffmpeg.wait().await;

    Ok(())
}

/// Reads audio chunks from ffmpeg stdout and sends them as binary WebSocket
/// frames to the Deepgram sink.
///
/// The `first_chunk` parameter contains audio data that was already read
/// during the startup handshake (to verify ffmpeg is producing output).
/// It is sent immediately before entering the main read loop.
///
/// Each subsequent chunk is `AUDIO_CHUNK_SIZE` bytes (250ms of 16kHz mono 16-bit PCM).
async fn send_audio_to_deepgram(
    mut ffmpeg_stdout: tokio::process::ChildStdout,
    mut ws_sink: futures_util::stream::SplitSink<DgWsStream, Message>,
    shutdown_rx: &mut watch::Receiver<bool>,
    first_chunk: Vec<u8>,
) -> Result<()> {
    // Send the pre-buffered first chunk immediately
    ws_sink
        .send(Message::Binary(first_chunk))
        .await
        .context("Failed to send first audio chunk to Deepgram")?;

    let mut buf = vec![0u8; AUDIO_CHUNK_SIZE];

    loop {
        tokio::select! {
            biased;

            _ = shutdown_rx.changed() => {
                debug!("Audio sender received shutdown signal");
                let close_msg = serde_json::json!({"type": "CloseStream"});
                let _ = ws_sink.send(Message::Text(close_msg.to_string())).await;
                break;
            }

            result = ffmpeg_stdout.read(&mut buf) => {
                match result {
                    Ok(0) => {
                        // EOF: stream ended or ffmpeg exited
                        info!("ffmpeg stdout EOF (stream may have ended)");
                        let close_msg = serde_json::json!({"type": "CloseStream"});
                        let _ = ws_sink.send(Message::Text(close_msg.to_string())).await;
                        break;
                    }
                    Ok(n) => {
                        ws_sink
                            .send(Message::Binary(buf[..n].to_vec()))
                            .await
                            .context("Failed to send audio to Deepgram")?;
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!("Failed to read from ffmpeg: {}", e));
                    }
                }
            }
        }
    }

    Ok(())
}

/// Reads transcription results from the Deepgram WebSocket stream and
/// forwards parsed events to the display channel.
async fn receive_deepgram_results(
    mut ws_stream: futures_util::stream::SplitStream<DgWsStream>,
    event_tx: mpsc::Sender<TranscriptionEvent>,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<()> {
    loop {
        tokio::select! {
            biased;

            _ = shutdown_rx.changed() => {
                debug!("Result receiver received shutdown signal");
                break;
            }

            msg = ws_stream.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Some(event) = parse_deepgram_response(&text) {
                            if event_tx.send(event).await.is_err() {
                                // Receiver dropped, main loop exited
                                break;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) => {
                        info!("Deepgram WebSocket closed");
                        break;
                    }
                    Some(Ok(_)) => {
                        // Ping/Pong/Binary frames - ignore
                    }
                    Some(Err(e)) => {
                        return Err(anyhow::anyhow!("Deepgram WebSocket error: {}", e));
                    }
                    None => {
                        info!("Deepgram WebSocket stream ended");
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_deepgram_final_result() {
        let json = r#"{
            "type": "Results",
            "channel": {
                "alternatives": [{"transcript": "hello world", "confidence": 0.98}]
            },
            "is_final": true,
            "speech_final": true
        }"#;

        let event = parse_deepgram_response(json);
        match event {
            Some(TranscriptionEvent::Final {
                transcript,
                confidence,
            }) => {
                assert_eq!(transcript, "hello world");
                assert!(confidence > 0.9);
            }
            other => panic!("Expected Final event, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_deepgram_is_final_produces_final() {
        let json = r#"{
            "type": "Results",
            "channel": {
                "alternatives": [{"transcript": "hello world", "confidence": 0.95}]
            },
            "is_final": true
        }"#;

        match parse_deepgram_response(json) {
            Some(TranscriptionEvent::Final { transcript, .. }) => {
                assert_eq!(transcript, "hello world");
            }
            other => panic!("Expected Final event, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_deepgram_interim_result() {
        let json = r#"{
            "type": "Results",
            "channel": {
                "alternatives": [{"transcript": "hello", "confidence": 0.85}]
            },
            "is_final": false,
            "speech_final": false
        }"#;

        match parse_deepgram_response(json) {
            Some(TranscriptionEvent::Interim { transcript }) => {
                assert_eq!(transcript, "hello");
            }
            other => panic!("Expected Interim event, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_deepgram_empty_transcript() {
        let json = r#"{
            "type": "Results",
            "channel": {
                "alternatives": [{"transcript": "", "confidence": 0.0}]
            },
            "is_final": true,
            "speech_final": true
        }"#;

        assert!(parse_deepgram_response(json).is_none());
    }

    #[test]
    fn test_parse_deepgram_short_transcript() {
        let json = r#"{
            "type": "Results",
            "channel": {
                "alternatives": [{"transcript": "a", "confidence": 0.5}]
            },
            "is_final": true
        }"#;

        // Single character filtered out as noise
        assert!(parse_deepgram_response(json).is_none());
    }

    #[test]
    fn test_parse_deepgram_non_results_type() {
        let json = r#"{"type": "Metadata", "request_id": "abc123"}"#;
        assert!(parse_deepgram_response(json).is_none());
    }

    #[test]
    fn test_parse_deepgram_malformed_json() {
        assert!(parse_deepgram_response("not json").is_none());
        assert!(parse_deepgram_response("{}").is_none());
    }

    #[test]
    fn test_parse_deepgram_missing_alternatives() {
        let json = r#"{
            "type": "Results",
            "channel": { "alternatives": [] },
            "is_final": true
        }"#;
        assert!(parse_deepgram_response(json).is_none());
    }

    #[test]
    fn test_parse_deepgram_whitespace_only_transcript() {
        let json = r#"{
            "type": "Results",
            "channel": {
                "alternatives": [{"transcript": "   ", "confidence": 0.5}]
            },
            "is_final": true
        }"#;

        // Whitespace-only filtered out after trim
        assert!(parse_deepgram_response(json).is_none());
    }

    #[test]
    fn test_parse_deepgram_missing_is_final_defaults_to_interim() {
        let json = r#"{
            "type": "Results",
            "channel": {
                "alternatives": [{"transcript": "test phrase", "confidence": 0.9}]
            }
        }"#;

        // Missing is_final defaults to false → Interim
        match parse_deepgram_response(json) {
            Some(TranscriptionEvent::Interim { transcript }) => {
                assert_eq!(transcript, "test phrase");
            }
            other => panic!("Expected Interim event, got {:?}", other),
        }
    }
}
