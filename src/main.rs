//! Twitch Chat Translator
//!
//! A real-time Twitch chat translator that detects non-English messages
//! and displays translations alongside the original text with colored output.

mod cache;
mod config;
mod translator;
mod twitch;

use anyhow::Result;
use colored::Colorize;
use config::Config;
use terminal_size::{terminal_size, Width};
use tracing::info;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use translator::Translator;
use twitch::TwitchClient;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging - use RUST_LOG env var, defaulting to info level
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("twitch_translator=info")),
        )
        .init();

    // Load configuration
    let config = Config::load()?;

    // Get terminal width for dynamic box sizing
    let term_width = terminal_size()
        .map(|(Width(w), _)| w as usize)
        .unwrap_or(80)
        .saturating_sub(1);

    // Build header box that fits terminal width
    let title = "Twitch Chat Translator";
    let subtitle = "Translating non-English messages to English";
    let inner_width = term_width.saturating_sub(2); // Account for box borders

    let top_border = format!("╔{}╗", "═".repeat(inner_width));
    let bottom_border = format!("╚{}╝", "═".repeat(inner_width));

    // Center the text within the box
    let title_padding = inner_width.saturating_sub(title.len()) / 2;
    let title_line = format!(
        "║{}{}{: <width$}║",
        " ".repeat(title_padding),
        title,
        "",
        width = inner_width - title_padding - title.len()
    );

    let subtitle_padding = inner_width.saturating_sub(subtitle.len()) / 2;
    let subtitle_line = format!(
        "║{}{}{: <width$}║",
        " ".repeat(subtitle_padding),
        subtitle,
        "",
        width = inner_width - subtitle_padding - subtitle.len()
    );

    println!("\n{}", top_border.bright_cyan().bold());
    println!("{}", title_line.bright_cyan().bold());
    println!("{}", subtitle_line.bright_cyan().bold());
    println!("{}\n", bottom_border.bright_cyan().bold());

    info!("Connecting to channel: #{}", config.channel);
    println!(
        "{} {}",
        "Connecting to:".bright_yellow(),
        format!("#{}", config.channel).bright_green().bold()
    );

    // Show translation mode
    if let Some(ref lang) = config.default_language {
        println!(
            "{} {} {}\n",
            "Mode:".bright_yellow(),
            "Translating ALL messages from".white(),
            lang.to_uppercase().bright_magenta().bold()
        );
    } else {
        println!(
            "{} {}\n",
            "Mode:".bright_yellow(),
            "Auto-detecting non-English messages".white()
        );
    }

    // Create translator and Twitch client
    let translator = Translator::new(
        &config.translation_api_url,
        config.translation_backend.clone(),
        config.cache_size_mb,
    )?;
    let mut client = TwitchClient::new(config, translator).await?;

    // Start processing chat messages
    client.run().await?;

    Ok(())
}
