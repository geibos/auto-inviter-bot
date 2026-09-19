# Builder: статическая musl-сборка (rustls, без OpenSSL)
FROM rust:1.96-alpine AS builder
RUN apk add --no-cache build-base
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
RUN cargo build --release --locked

# Runtime: alpine вместо scratch — ca-certificates, tzdata и шелл для отладки
FROM alpine:3.22
RUN apk add --no-cache ca-certificates tzdata
COPY --from=builder /app/target/release/auto-inviter-bot /usr/local/bin/auto-inviter-bot
ENV DATABASE_PATH=/data/bot.db
VOLUME /data
ENTRYPOINT ["/usr/local/bin/auto-inviter-bot"]
