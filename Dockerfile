# Static musl build, small runtime. Multi-arch via `docker buildx build --platform linux/amd64,linux/arm64`.
FROM rust:1.98-alpine AS build
RUN apk add --no-cache musl-dev build-base
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p graph-sentinel && strip target/release/graph-sentinel

FROM alpine:3.22
RUN apk add --no-cache ca-certificates && adduser -D -u 10001 sentinel
COPY --from=build /src/target/release/graph-sentinel /usr/local/bin/graph-sentinel
USER sentinel
WORKDIR /data
ENV GRAPH_SENTINEL_CONFIG=/etc/graph-sentinel/graph-sentinel.toml \
    GRAPH_SENTINEL_OUTPUTS__STATE_DB=/data/graph-sentinel.sqlite \
    GRAPH_SENTINEL_OUTPUTS__LISTEN=0.0.0.0:9477
EXPOSE 9477
VOLUME ["/data"]
ENTRYPOINT ["graph-sentinel"]
