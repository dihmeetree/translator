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
RUN cargo build --release --bin translation-api && \
    rm -rf src

# Copy actual source code
COPY src ./src

# Touch files to invalidate cache and rebuild
RUN touch src/api.rs && cargo build --release --bin translation-api

# Runtime stage
FROM alpine:3.23.0

# Install runtime dependencies
RUN apk add --no-cache ca-certificates

WORKDIR /app

# Copy the binary from builder
COPY --from=builder /app/target/release/translation-api /app/translation-api

# Default port and host (0.0.0.0 to be accessible from outside container)
ENV PORT=4000
ENV HOST=0.0.0.0

EXPOSE 4000

CMD ["/app/translation-api"]
