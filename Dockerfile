# builder
FROM rust:1-bookworm AS build
WORKDIR /src

COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release && cp target/release/worker /worker

# distroless runner
FROM gcr.io/distroless/cc-debian12

COPY --from=build /worker /usr/local/bin/worker

# All worker config knobs are env vars. Override at `docker run -e KEY=val`
# or via `--env-file`. Set AUTH_TOKEN to enable bearer-token auth on /init;
# leave it unset to disable auth entirely.
ENV BIND_ADDR=0.0.0.0:3000 \
    WASM_FILES_DIR=/data/wasm_files \
    POOL_INSTANCES=8192 \
    WASM_LOGS=0 \
    STATS_LOG=0
# Persist enrolled wasm components here. Mount with `-v` to keep them across
# container restarts.
VOLUME ["/data/wasm_files"]

EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/worker"]
