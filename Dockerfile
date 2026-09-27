# syntax=docker/dockerfile:1
#
# SenClaw daemon in a container.
#
# The daemon links no inference code at all now (docs/runtime-protocol.md):
# every engine — MLX, llama.cpp, decision, OCR, Whisper, TTS — is a separate
# `sen-*` runtime the daemon installs and supervises as a child process. This
# image builds and ships only the daemon binary and the Web UI; install
# runtimes at container runtime with `senclaw runtime install <id>` (into the
# `~/.senclaw` volume, so they survive a restart) or bind-mount a
# pre-populated `~/.senclaw/runtimes`.
#
# Build context is the **umbrella directory** (the parent of this repo and of
# `web-app/`), not this repo alone — the Web UI moved to its own repository:
#
#   cd /path/to/SenClaw && docker build -f senclaw/Dockerfile -t senclaw .

# ===== 1. Web UI (sibling repo) =====
FROM node:20-bookworm-slim AS web
WORKDIR /src/web-app
# Lockfile first: the dependency layer survives every source-only change.
COPY web-app/package.json web-app/package-lock.json ./
RUN npm ci
COPY web-app/ ./
RUN npm run build

# ===== 2. Daemon =====
FROM rust:1-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev clang cmake \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY senclaw/ .
# Default features — the daemon compiles no model/inference code on any
# platform now, so there is nothing Apple-only to accidentally pull in here.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --bin senclaw && \
    cp target/release/senclaw /usr/local/bin/senclaw

# ===== 3. Runtime =====
FROM debian:bookworm-slim

# git: the wiki and pattern sources are git checkouts.
# python3/nodejs: Space Apps declaring those runners. Drop them if you run no
#   such app — nothing else in the daemon needs them.
# curl: the healthcheck below.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates git curl \
        python3 python3-venv \
        nodejs npm \
    && rm -rf /var/lib/apt/lists/*

# Never root: an app the daemon launches inherits this user.
RUN useradd --create-home --shell /bin/bash senclaw
USER senclaw
ENV HOME=/home/senclaw
WORKDIR /home/senclaw

COPY --from=build /usr/local/bin/senclaw /usr/local/bin/senclaw
COPY --from=web /src/web-app/dist /app/web/dist
ENV SENCLAW_WEB_DIST=/app/web/dist

# The container's own network namespace — `-p` is what decides who can reach
# it. This is NOT the Space-App knob (`SENCLAW_BIND_HOST`), which must stay
# loopback: apps have no authentication of their own.
ENV SENCLAW_UI_BIND_HOST=0.0.0.0

# `always`, not `auto`: with `--network host`, or a reverse proxy in the same
# pod, requests arrive from 127.0.0.1 and `auto` would exempt every one of
# them. Set SENCLAW_AUTH_MODE=auto at run time only if you know the port is
# unreachable from anywhere you do not trust.
ENV SENCLAW_AUTH_MODE=always

# Everything stateful — database, config, uploads, and the generated
# `api_token` — lives here. Without a volume the token is new on every
# restart, which invalidates every saved login and reads as a broken sign-in.
VOLUME ["/home/senclaw/.senclaw"]

EXPOSE 18788 18789

# `/api/auth/status` is one of the two unauthenticated routes. Any other path
# answers 401 under `always` and would flap the container unhealthy forever.
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD curl -fsS http://127.0.0.1:18788/api/auth/status || exit 1

ENTRYPOINT ["senclaw"]
