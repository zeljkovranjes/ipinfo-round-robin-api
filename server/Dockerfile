FROM rust:alpine AS builder
WORKDIR /app
COPY . .
RUN apk add --no-cache musl-dev && cargo build --release

FROM scratch
COPY --from=builder /app/target/release/ipinfo-round-robin-api /server
ENTRYPOINT ["/server"]
