# Twitch Chat Translator

A real-time Twitch chat translator that displays non-English messages with translations and romanization (pronunciation). Perfect for learning languages while watching international streams.

## Features

- **Real-time translation** - Translates chat messages as they come in
- **Romanization** - Shows pronunciation from Google Translate (e.g., "привет" → "privet")
- **Smart caching** - SQLite-based cache with intelligent key extraction (strips ASCII prefixes/suffixes, case-insensitive)
- **Language detection** - Automatically detects non-English messages
- **Colored output** - Usernames colored based on Twitch settings
- **Badge support** - Shows colored badges: [B] for broadcasters, [M] for mods, [V] for VIPs
- **Reply context** - Shows what message a reply is responding to
- **Loading spinner** - Visual feedback while translations are being fetched
- **Scalable API** - Deploy translation servers to multiple locations to bypass rate limits

## Example Output

```
[B] StreamerName: привет всем как дела
  ♪ privet vsem kak dela
  ↳ [Russian] hello everyone how are you
────────────────────────────────────────────────────────────────
[M] ModeratorName: スタートしましょう
  ♪ Sutātoshimashou
  ↳ [Japanese] let's start
────────────────────────────────────────────────────────────────
  ┌─ replying to StreamerName "привет всем как дела"
RegularUser: отлично!
  ♪ otlichno!
  ↳ [Russian] great!
────────────────────────────────────────────────────────────────
```

## Architecture

```
┌─────────────────────┐      ┌─────────────────────┐      ┌─────────────────────┐
│  twitch-translator  │ ───► │   translation-api   │ ───► │   Google Translate  │
│     (your PC)       │ HTTP │   (Docker/servers)  │ HTTP │   (googleapis.com)  │
└─────────────────────┘      └─────────────────────┘      └─────────────────────┘
```

The system consists of two components:

1. **twitch-translator** - Connects to Twitch chat, detects languages, displays translations
2. **translation-api** - HTTP server that proxies requests to Google Translate (deploy to bypass rate limits)

### Translation Cache

Translations are cached in a local SQLite database to avoid redundant API calls:

- **Location**: `./translation_cache.db` (current directory)
- **Size limit**: Configurable via `CACHE_SIZE_MB` (default 50MB), oldest entries automatically purged when exceeded
- **Persistence**: Cache survives restarts, so repeated messages are instant
- **Smart caching**:
  - Strips ASCII prefixes/suffixes before caching (e.g., "5 снайперов" and "10 снайперов" share cached "снайперов" → "snipers")
  - Case-insensitive lookups (e.g., "ПРИВЕТ" and "привет" use the same cache entry)
  - Skips ASCII-only messages (English text, numbers, punctuation, emojis)

## Installation

### Prerequisites

- Rust 1.70+
- Docker (optional, for deploying translation API)

### Build

```bash
cargo build --release
```

Binaries will be at:

- `target/release/twitch-translator` - Twitch chat client
- `target/release/translation-api` - Translation API server

## Quick Start

### 1. Start the Translation API Server

**Option A: Run locally**

```bash
cargo run --release --bin translation-api
```

**Option B: Run with Docker**

```bash
docker build -t translation-api .
docker run -d -p 4000:4000 translation-api
```

### 2. Configure Environment

```bash
cp .env.example .env
```

Edit `.env`:

```bash
TWITCH_CHANNEL=channel_name
TRANSLATION_API_URL=http://localhost:4000
DEFAULT_LANGUAGE=ru  # Optional: force translate all messages from Russian
```

### 3. Run the Translator

```bash
cargo run --release --bin twitch-translator
```

## Configuration

### Environment Variables

| Variable               | Description                              | Default                 |
| ---------------------- | ---------------------------------------- | ----------------------- |
| `TWITCH_CHANNEL`       | Channel to join (required)               | -                       |
| `TRANSLATION_BACKEND`  | `google` or `libretranslate`             | `google`                |
| `TRANSLATION_API_URL`  | URL of translation API server            | `http://localhost:4000` |
| `DEFAULT_LANGUAGE`     | Force source language (e.g., `ru`, `ja`) | Auto-detect             |
| `DETECTION_CONFIDENCE` | Min confidence for detection (0.0-1.0)   | `0.5`                   |
| `TWITCH_USERNAME`      | Custom username                          | Anonymous               |
| `TWITCH_OAUTH_TOKEN`   | OAuth token for authenticated access     | Anonymous               |
| `CACHE_SIZE_MB`        | Maximum cache size in megabytes          | `50`                    |

### Translation API Server

| Variable   | Description                       | Default |
| ---------- | --------------------------------- | ------- |
| `PORT`     | HTTP server port                  | `4000`  |
| `RUST_LOG` | Log level (e.g., `info`, `debug`) | `info`  |

## Translation API

The translation API server provides a simple HTTP interface for translation.

### Endpoints

#### `GET /translate/:source/:target/:text`

Translates text and returns both translation and romanization.

**Parameters:**

- `source` - Source language code (e.g., `ru`, `ja`, `zh`, or `auto`)
- `target` - Target language code (e.g., `en`)
- `text` - URL-encoded text to translate

**Response:**

```json
{
  "translation": "Hello",
  "romanization": "privet"
}
```

**Example:**

```bash
curl "http://localhost:4000/translate/ru/en/привет"
# {"translation":"Hello","romanization":"privet"}
```

#### `GET /health`

Health check endpoint. Returns `OK`.

## Scaling for Rate Limits

Google Translate may rate limit requests from a single IP. To bypass this, deploy the translation API to multiple servers:

### Deploy to Multiple Servers

```bash
# Build and push Docker image
docker build -f Dockerfile.api -t your-registry/translation-api .
docker push your-registry/translation-api

# Deploy to server 1
ssh server1 "docker run -d -p 4000:4000 your-registry/translation-api"

# Deploy to server 2
ssh server2 "docker run -d -p 4000:4000 your-registry/translation-api"
```

### Configure Load Balancing

You can either:

1. **Use a load balancer** (nginx, HAProxy, etc.) in front of your API servers
2. **Rotate URLs manually** by changing `TRANSLATION_API_URL`

Example nginx config:

```nginx
upstream translation_api {
    server server1:4000;
    server server2:4000;
}

server {
    listen 80;
    location / {
        proxy_pass http://translation_api;
    }
}
```

## Alternative Backend: LibreTranslate

If you prefer open-source translation without Google, you can use LibreTranslate:

```bash
# Start LibreTranslate
docker run -d -p 5000:5000 libretranslate/libretranslate --load-only ru,en

# Configure
TRANSLATION_BACKEND=libretranslate
TRANSLATION_API_URL=http://localhost:5000/translate
```

**Note:** LibreTranslate has lower translation quality and doesn't provide romanization.

## Language Codes

Common language codes for `DEFAULT_LANGUAGE`:

| Code | Language   |
| ---- | ---------- |
| `ru` | Russian    |
| `es` | Spanish    |
| `fr` | French     |
| `de` | German     |
| `ja` | Japanese   |
| `ko` | Korean     |
| `zh` | Chinese    |
| `pt` | Portuguese |
| `it` | Italian    |
| `pl` | Polish     |
| `uk` | Ukrainian  |
| `ar` | Arabic     |

## License

MIT
