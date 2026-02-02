//! Translation module supporting direct Google Translate API and LibreTranslate.

use crate::cache::TranslationCache;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use tracing::{debug, warn};
use whatlang::{detect, Lang};

/// Result of language detection on a text.
#[derive(Debug, Clone)]
pub struct DetectionResult {
    /// The detected language code (e.g., "es", "fr", "de").
    pub language: String,

    /// The full language name (e.g., "Spanish", "French", "German").
    pub language_name: String,

    /// Confidence score from 0.0 to 1.0.
    pub confidence: f64,

    /// Whether the text is detected as English.
    pub is_english: bool,
}

/// Result of a translation request.
#[derive(Debug, Clone)]
pub struct TranslationResult {
    /// The translated text.
    pub translation: String,

    /// Romanization/pronunciation of the source text (if available).
    pub romanization: Option<String>,
}

/// Translation backend to use.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum TranslationBackend {
    /// Google Translate via your translation-api proxy server.
    #[default]
    Google,
    /// LibreTranslate - open-source translation (lower quality, no pronunciation).
    LibreTranslate,
}

impl FromStr for TranslationBackend {
    type Err = std::convert::Infallible;

    /// Parses the backend from a string.
    ///
    /// Accepts "libretranslate" or "libre" for LibreTranslate, defaults to Google.
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match s.to_lowercase().as_str() {
            "libretranslate" | "libre" => TranslationBackend::LibreTranslate,
            _ => TranslationBackend::Google,
        })
    }
}

/// Translation service that detects languages and translates to English.
///
/// Includes a persistent SQLite cache to avoid redundant API calls.
#[derive(Clone)]
pub struct Translator {
    /// HTTP client for API requests.
    client: reqwest::Client,

    /// Base URL for the translation API (used for LibreTranslate, ignored for Google).
    api_url: String,

    /// Which backend to use.
    backend: TranslationBackend,

    /// Persistent cache for translation results (shared across clones).
    cache: TranslationCache,
}

/// Request body for LibreTranslate API.
#[derive(Debug, Serialize)]
struct LibreTranslateRequest<'a> {
    q: &'a str,
    source: &'a str,
    target: &'a str,
}

/// Response from LibreTranslate API.
#[derive(Debug, Deserialize)]
struct LibreTranslateResponse {
    #[serde(rename = "translatedText")]
    translated_text: String,
}

impl Translator {
    /// Creates a new Translator instance with persistent SQLite cache.
    ///
    /// # Arguments
    ///
    /// * `api_url` - The base URL of the translation API (for LibreTranslate).
    /// * `backend` - Which translation backend to use.
    /// * `cache_size_mb` - Maximum cache size in megabytes.
    ///
    /// # Errors
    ///
    /// Returns an error if the cache database cannot be initialized.
    pub fn new(api_url: &str, backend: TranslationBackend, cache_size_mb: u64) -> Result<Self> {
        let cache_size_bytes = cache_size_mb * 1024 * 1024;
        let cache = TranslationCache::with_max_size(cache_size_bytes)?;

        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("Failed to create HTTP client"),
            api_url: api_url.to_string(),
            backend,
            cache,
        })
    }

    /// Detects the language of the given text.
    ///
    /// Uses the `whatlang` library for fast local language detection.
    ///
    /// # Arguments
    ///
    /// * `text` - The text to analyze.
    ///
    /// # Returns
    ///
    /// A `DetectionResult` containing the detected language and confidence,
    /// or `None` if detection failed or confidence is too low.
    pub fn detect_language(&self, text: &str) -> Option<DetectionResult> {
        // Skip very short messages as detection is unreliable
        if text.len() < 3 {
            return None;
        }

        let info = detect(text)?;
        let lang = info.lang();
        let confidence = info.confidence();

        let (language_code, language_name) = lang_to_code_and_name(lang);
        let is_english = lang == Lang::Eng;

        debug!(
            "Detected language: {} ({}) with confidence {:.2}",
            language_name, language_code, confidence
        );

        Some(DetectionResult {
            language: language_code.to_string(),
            language_name: language_name.to_string(),
            confidence,
            is_english,
        })
    }

    /// Translates text to English, returning both translation and romanization.
    ///
    /// Results are cached in a persistent SQLite database to avoid redundant API calls.
    /// Skips translation for text that is only numbers, punctuation, or ASCII/English text.
    ///
    /// Uses smart caching: strips leading/trailing ASCII portions and caches only the
    /// non-ASCII core, then reconstructs the full translation. This allows cache hits
    /// for "5 снайперов" and "10 снайперов" to both use cached "снайперов" → "snipers".
    ///
    /// # Arguments
    ///
    /// * `text` - The text to translate.
    /// * `source_lang` - The source language code (e.g., "es", "fr").
    ///
    /// # Returns
    ///
    /// A `TranslationResult` containing the translated text and optional romanization,
    /// or `None` if the text doesn't need translation.
    pub async fn translate(
        &self,
        text: &str,
        source_lang: &str,
    ) -> Result<Option<TranslationResult>> {
        // Skip if text is only ASCII (English text, numbers, punctuation, emojis)
        if !needs_translation(text) {
            return Ok(None);
        }

        // Extract leading/trailing ASCII portions for smart caching
        let (prefix, core, suffix) = extract_ascii_affixes(text);

        // Lowercase the core for case-insensitive cache lookups
        // This improves cache hits: "АХахахаха" and "ахахахаха" use same entry
        let core_lower = core.to_lowercase();

        // Check cache for the core (non-ASCII) portion
        if let Some(cached) = self.cache.get(&core_lower, source_lang) {
            // Reconstruct full translation with original ASCII prefix/suffix.
            // Fast path avoids allocation when affixes are empty (the common case).
            let full_translation = reconstruct_with_affixes(prefix, &cached.translation, suffix);
            let full_romanization = cached
                .romanization
                .map(|r| reconstruct_with_affixes(prefix, &r, suffix));
            return Ok(Some(TranslationResult {
                translation: full_translation,
                romanization: full_romanization,
            }));
        }

        // Perform the translation on the core portion only (use original case for API)
        let result = match self.backend {
            TranslationBackend::Google => self.translate_with_google(core, source_lang).await,
            TranslationBackend::LibreTranslate => {
                self.translate_with_libretranslate(core, source_lang).await
            }
        }?;

        // Cache the core result with lowercase key (ignore errors, caching is best-effort)
        if let Err(e) = self.cache.insert(
            &core_lower,
            source_lang,
            &result.translation,
            result.romanization.as_deref(),
        ) {
            warn!("Failed to cache translation: {}", e);
        }

        // Reconstruct full translation with original ASCII prefix/suffix.
        // Fast path avoids allocation when affixes are empty (the common case).
        let full_translation = reconstruct_with_affixes(prefix, &result.translation, suffix);
        let full_romanization = result
            .romanization
            .map(|r| reconstruct_with_affixes(prefix, &r, suffix));

        Ok(Some(TranslationResult {
            translation: full_translation,
            romanization: full_romanization,
        }))
    }

    /// Translates text using your translation-api proxy server.
    ///
    /// The proxy server calls Google Translate and returns both translation and romanization.
    async fn translate_with_google(
        &self,
        text: &str,
        source_lang: &str,
    ) -> Result<TranslationResult> {
        let encoded_text = urlencoding::encode(text);

        // Build URL: {api_url}/translate/{source}/{target}/{text}
        let url = format!(
            "{}/translate/{}/en/{}",
            self.api_url.trim_end_matches('/'),
            source_lang,
            encoded_text
        );

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to send request to translation API")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            warn!("Translation API error: {} - {}", status, body);
            anyhow::bail!("Translation API returned status: {}", status);
        }

        #[derive(Deserialize)]
        struct ApiResponse {
            translation: String,
            romanization: Option<String>,
        }

        let result: ApiResponse = response
            .json()
            .await
            .context("Failed to parse translation API response")?;

        debug!(
            "Google Translate: '{}' -> '{}' (romanization: {:?})",
            text, result.translation, result.romanization
        );

        Ok(TranslationResult {
            translation: result.translation,
            romanization: result.romanization,
        })
    }

    /// Translates text using the LibreTranslate API.
    async fn translate_with_libretranslate(
        &self,
        text: &str,
        source_lang: &str,
    ) -> Result<TranslationResult> {
        let request = LibreTranslateRequest {
            q: text,
            source: source_lang,
            target: "en",
        };

        let response = self
            .client
            .post(&self.api_url)
            .json(&request)
            .send()
            .await
            .context("Failed to send request to LibreTranslate API")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            warn!("LibreTranslate API error: {} - {}", status, body);
            anyhow::bail!("LibreTranslate API returned status: {}", status);
        }

        let result: LibreTranslateResponse = response
            .json()
            .await
            .context("Failed to parse LibreTranslate API response")?;

        Ok(TranslationResult {
            translation: result.translated_text,
            romanization: None, // LibreTranslate doesn't provide romanization
        })
    }
}

/// Converts a `whatlang::Lang` to its ISO 639-1 code and English name.
fn lang_to_code_and_name(lang: Lang) -> (&'static str, &'static str) {
    match lang {
        Lang::Afr => ("af", "Afrikaans"),
        Lang::Ara => ("ar", "Arabic"),
        Lang::Bul => ("bg", "Bulgarian"),
        Lang::Ben => ("bn", "Bengali"),
        Lang::Cat => ("ca", "Catalan"),
        Lang::Ces => ("cs", "Czech"),
        Lang::Cmn => ("zh", "Chinese"),
        Lang::Dan => ("da", "Danish"),
        Lang::Deu => ("de", "German"),
        Lang::Ell => ("el", "Greek"),
        Lang::Eng => ("en", "English"),
        Lang::Epo => ("eo", "Esperanto"),
        Lang::Spa => ("es", "Spanish"),
        Lang::Est => ("et", "Estonian"),
        Lang::Fin => ("fi", "Finnish"),
        Lang::Fra => ("fr", "French"),
        Lang::Heb => ("he", "Hebrew"),
        Lang::Hin => ("hi", "Hindi"),
        Lang::Hrv => ("hr", "Croatian"),
        Lang::Hun => ("hu", "Hungarian"),
        Lang::Ind => ("id", "Indonesian"),
        Lang::Ita => ("it", "Italian"),
        Lang::Jpn => ("ja", "Japanese"),
        Lang::Kat => ("ka", "Georgian"),
        Lang::Kor => ("ko", "Korean"),
        Lang::Lat => ("la", "Latin"),
        Lang::Lav => ("lv", "Latvian"),
        Lang::Lit => ("lt", "Lithuanian"),
        Lang::Mkd => ("mk", "Macedonian"),
        Lang::Nld => ("nl", "Dutch"),
        Lang::Nob => ("nb", "Norwegian"),
        Lang::Pol => ("pl", "Polish"),
        Lang::Por => ("pt", "Portuguese"),
        Lang::Ron => ("ro", "Romanian"),
        Lang::Rus => ("ru", "Russian"),
        Lang::Slk => ("sk", "Slovak"),
        Lang::Slv => ("sl", "Slovenian"),
        Lang::Swe => ("sv", "Swedish"),
        Lang::Tam => ("ta", "Tamil"),
        Lang::Tha => ("th", "Thai"),
        Lang::Tur => ("tr", "Turkish"),
        Lang::Ukr => ("uk", "Ukrainian"),
        Lang::Urd => ("ur", "Urdu"),
        Lang::Vie => ("vi", "Vietnamese"),
        _ => ("auto", "Unknown"),
    }
}

/// Checks if text contains non-ASCII characters that need translation.
///
/// Returns `false` for:
/// - Empty or whitespace-only text
/// - Numbers only (with optional punctuation)
/// - ASCII-only text (English letters, numbers, punctuation, emojis)
///
/// Returns `true` if the text contains non-ASCII characters (Cyrillic, CJK, etc.)
fn needs_translation(text: &str) -> bool {
    let trimmed = text.trim();

    // Skip empty text
    if trimmed.is_empty() {
        return false;
    }

    // Fast path: byte-level ASCII check avoids char iteration for
    // all-ASCII messages, which are the majority of Twitch chat.
    if trimmed.is_ascii() {
        return false;
    }

    // Check if any character is non-ASCII and not an emoji
    // Emojis are in ranges like U+1F000-U+1FFFF, U+2600-U+26FF, etc.
    trimmed.chars().any(|c| !c.is_ascii() && !is_emoji(c))
}

/// Checks if a character is likely an emoji.
///
/// This is a simplified check covering common emoji ranges.
fn is_emoji(c: char) -> bool {
    let code = c as u32;
    // Common emoji ranges
    matches!(code,
        0x1F300..=0x1F9FF |  // Miscellaneous Symbols and Pictographs, Emoticons, etc.
        0x2600..=0x26FF |    // Miscellaneous Symbols
        0x2700..=0x27BF |    // Dingbats
        0xFE00..=0xFE0F |    // Variation Selectors
        0x1F000..=0x1F02F |  // Mahjong, Domino
        0x1FA00..=0x1FAFF    // Chess, symbols
    )
}

/// Extracts leading and trailing ASCII portions from text.
///
/// This enables smarter caching by separating the non-ASCII "core" that needs
/// translation from ASCII prefixes/suffixes (numbers, punctuation) that don't.
///
/// # Examples
///
/// - `"5 снайперов"` → `("5 ", "снайперов", "")`
/// - `"снайперов!!!"` → `("", "снайперов", "!!!")`
/// - `"10 привет 20"` → `("10 ", "привет", " 20")`
/// - `"hello"` → `("", "hello", "")` (all ASCII, returned as core)
///
/// # Returns
///
/// A tuple of `(prefix, core, suffix)` where:
/// - `prefix`: Leading ASCII characters (including spaces)
/// - `core`: The non-ASCII portion to be translated/cached
/// - `suffix`: Trailing ASCII characters (including spaces)
fn extract_ascii_affixes(text: &str) -> (&str, &str, &str) {
    if text.is_empty() {
        return ("", "", "");
    }

    // Find the byte offset of the first non-ASCII, non-emoji character.
    // Uses char_indices() directly on &str to avoid allocating a Vec<char>.
    let first_byte = text
        .char_indices()
        .find(|(_, c)| !c.is_ascii() && !is_emoji(*c))
        .map(|(i, _)| i);

    // If no non-ASCII found, return entire text as core (will likely be skipped by needs_translation)
    let first_byte = match first_byte {
        Some(i) => i,
        None => return ("", text, ""),
    };

    // Find the byte offset just past the last non-ASCII, non-emoji character.
    let (last_byte_start, last_char) = text
        .char_indices()
        .rev()
        .find(|(_, c)| !c.is_ascii() && !is_emoji(*c))
        .expect("guaranteed by first_byte existing");
    let last_byte_end = last_byte_start + last_char.len_utf8();

    let prefix = &text[..first_byte];
    let core = &text[first_byte..last_byte_end];
    let suffix = &text[last_byte_end..];

    (prefix, core, suffix)
}

/// Reconstructs a string by joining a prefix, core, and suffix.
///
/// Avoids a `format!` allocation when both affixes are empty, which is
/// the common case for purely non-ASCII text like "привет".
fn reconstruct_with_affixes(prefix: &str, core: &str, suffix: &str) -> String {
    if prefix.is_empty() && suffix.is_empty() {
        core.to_string()
    } else {
        format!("{}{}{}", prefix, core, suffix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_ascii_affixes_leading_number() {
        let (prefix, core, suffix) = extract_ascii_affixes("5 снайперов");
        assert_eq!(prefix, "5 ");
        assert_eq!(core, "снайперов");
        assert_eq!(suffix, "");
    }

    #[test]
    fn test_extract_ascii_affixes_trailing_punctuation() {
        let (prefix, core, suffix) = extract_ascii_affixes("снайперов!!!");
        assert_eq!(prefix, "");
        assert_eq!(core, "снайперов");
        assert_eq!(suffix, "!!!");
    }

    #[test]
    fn test_extract_ascii_affixes_both_sides() {
        let (prefix, core, suffix) = extract_ascii_affixes("10 привет 20");
        assert_eq!(prefix, "10 ");
        assert_eq!(core, "привет");
        assert_eq!(suffix, " 20");
    }

    #[test]
    fn test_extract_ascii_affixes_no_affixes() {
        let (prefix, core, suffix) = extract_ascii_affixes("привет");
        assert_eq!(prefix, "");
        assert_eq!(core, "привет");
        assert_eq!(suffix, "");
    }

    #[test]
    fn test_extract_ascii_affixes_all_ascii() {
        let (prefix, core, suffix) = extract_ascii_affixes("hello world");
        assert_eq!(prefix, "");
        assert_eq!(core, "hello world");
        assert_eq!(suffix, "");
    }

    #[test]
    fn test_extract_ascii_affixes_empty() {
        let (prefix, core, suffix) = extract_ascii_affixes("");
        assert_eq!(prefix, "");
        assert_eq!(core, "");
        assert_eq!(suffix, "");
    }

    #[test]
    fn test_extract_ascii_affixes_complex() {
        let (prefix, core, suffix) = extract_ascii_affixes("123 тест test");
        assert_eq!(prefix, "123 ");
        assert_eq!(core, "тест");
        assert_eq!(suffix, " test");
    }

    #[test]
    fn test_needs_translation_ascii_only() {
        assert!(!needs_translation("hello world"));
        assert!(!needs_translation("123"));
        assert!(!needs_translation("!@#$%"));
        assert!(!needs_translation("   "));
        assert!(!needs_translation(""));
    }

    #[test]
    fn test_needs_translation_cyrillic() {
        assert!(needs_translation("привет"));
        assert!(needs_translation("5 снайперов"));
        assert!(needs_translation("hello мир"));
    }

    #[test]
    fn test_needs_translation_with_emoji() {
        // Emoji alone should not trigger translation
        assert!(!needs_translation("😀"));
        assert!(!needs_translation("hello 😀"));
        // But non-ASCII text with emoji should
        assert!(needs_translation("привет 😀"));
    }
}
