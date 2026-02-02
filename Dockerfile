# Build stage
FROM rust:1.91.1-alpine AS builder

# Install build dependencies
RUN apk add --no-cache musl-dev pkgconfig openssl-dev openssl-libs-static

WORKDIR /app

# Copy only Cargo files first for better caching
COPY Cargo.toml Cargo.lock ./

# Create dummy source files to build dependencies
RUN mkdir src && \
    echo 'fn main() {}' > src/main.rs && \
    echo 'fn main() {}' > src/api.rs && \
    touch src/lib.rs

# Build dependencies only
RUN cargo build --release 2>/dev/null || true && \
    rm -rf src

# Copy actual source code
COPY src ./src

# Touch files to invalidate cache and rebuild
RUN touch src/main.rs && \
    cargo build --release --bin twitch-translator

# Runtime stage
FROM alpine:3.23.0

# Install runtime dependencies:
#   ca-certificates - TLS for outbound HTTPS/WSS connections
#   ffmpeg          - decodes stream audio to PCM for Deepgram STT
#   python3 + pip   - required to install streamlink
#   streamlink      - extracts HLS audio stream from Twitch
RUN apk add --no-cache ca-certificates ffmpeg python3 py3-pip && \
    pip3 install --no-cache-dir --break-system-packages streamlink

WORKDIR /app

# Copy the binary from builder
COPY --from=builder /app/target/release/twitch-translator /app/twitch-translator

# Web UI port
EXPOSE 3000

CMD ["/app/twitch-translator"]
