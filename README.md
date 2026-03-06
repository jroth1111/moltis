<div align="center">

<a href="https://moltis.org"><img src="https://raw.githubusercontent.com/moltis-org/moltis-website/main/favicon.svg" alt="Moltis" width="64"></a>

# Moltis — A Rust-native claw you can trust

One binary — sandboxed, secure, yours.

[![CI](https://github.com/moltis-org/moltis/actions/workflows/ci.yml/badge.svg)](https://github.com/moltis-org/moltis/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/moltis-org/moltis/graph/badge.svg)](https://codecov.io/gh/moltis-org/moltis)
[![CodSpeed](https://img.shields.io/endpoint?url=https://codspeed.io/badge.json&style=flat&label=CodSpeed)](https://codspeed.io/moltis-org/moltis)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.91%2B-orange.svg)](https://www.rust-lang.org)
[![Discord](https://img.shields.io/discord/1469505370169933837?color=5865F2&label=Discord&logo=discord&logoColor=white)](https://discord.gg/XnmrepsXp5)

[Installation](#installation) • [Comparison](#comparison) • [Architecture](#architecture--crate-map) • [Security](#security) • [Features](#features) • [How It Works](#how-it-works) • [Contributing](CONTRIBUTING.md)

</div>

---

Moltis recently hit [the front page of Hacker News](https://news.ycombinator.com/item?id=46993587). Please [open an issue](https://github.com/moltis-org/moltis/issues) for any friction at all. I'm focused on making Moltis excellent.

**Secure by design** — Your keys never leave your machine. Every command runs in a sandboxed container, never on your host.

**Your hardware** — Runs on a Mac Mini, a Raspberry Pi, or any server you own. One Rust binary, no Node.js, no npm, no runtime.

**Full-featured** — Voice, memory, scheduling, Telegram, Discord, browser automation, MCP servers — all built-in. No plugin marketplace to get supply-chain attacked through.

**Auditable** — The runtime is split into dedicated Rust crates for the gateway, agents, tools, providers, sessions, memory, channels, and security. The core path stays in plain Rust with zero `unsafe` code\*.

## Installation

```bash
# One-liner install script (macOS / Linux)
curl -fsSL https://www.moltis.org/install.sh | sh

# macOS / Linux via Homebrew
brew install moltis-org/tap/moltis

# Docker (multi-arch: amd64/arm64)
docker pull ghcr.io/moltis-org/moltis:latest

# Or build from source
cargo install moltis --git https://github.com/moltis-org/moltis
```

## Comparison

| | OpenClaw | PicoClaw | NanoClaw | ZeroClaw | **Moltis** |
|---|---|---|---|---|---|
| Language | TypeScript | Go | TypeScript | Rust | **Rust** |
| Code shape | Broad JS app | Narrow CLI | Minimal JS app | Small Rust app | **Layered Rust workspace** |
| Runtime | Node.js + npm | Single binary | Node.js | Single binary | **Single binary** |
| Sandbox | App-level | — | Docker | Docker | **Docker + Apple Container** |
| Memory safety | GC | GC | GC | Ownership | **Ownership, zero `unsafe`\*** |
| Auth | Basic | API keys | None | Token + OAuth | **Password + Passkey + API keys + Vault** |
| Voice I/O | Plugin | — | — | — | **Built-in** |
| MCP | Yes | — | — | — | **Yes (stdio + HTTP/SSE)** |
| Hooks | Yes (limited) | — | — | — | **Yes** |
| Skills | Yes (store) | Yes | Yes | Yes | **Yes (+ OpenClaw Store)** |
| Memory/RAG | Plugin | — | Per-group | SQLite + FTS | **SQLite + FTS + vector** |

\* `unsafe` is denied workspace-wide. The only exceptions are opt-in FFI wrappers behind the `local-embeddings` feature flag, not part of the core.

> [Full comparison with benchmarks →](https://docs.moltis.org/comparison.html)

## Architecture — Crate Map

**Core runtime**

- `moltis` provides the CLI entrypoint and operational commands.
- `moltis-gateway` owns the HTTP/WebSocket server, auth, lifecycle wiring, and service registration.
- `moltis-chat`, `moltis-agents`, `moltis-tools`, and `moltis-providers` handle chat orchestration, the agent loop, tool execution, and provider adapters.
- `moltis-config`, `moltis-sessions`, `moltis-common`, `moltis-protocol`, and `moltis-service-traits` provide configuration, persistence, shared types, wire contracts, and service boundaries.

**Additive crates**

- Web and API: `moltis-web`, `moltis-graphql`
- Memory and retrieval: `moltis-memory`, `moltis-qmd`
- Channels: `moltis-channels`, `moltis-telegram`, `moltis-discord`, `moltis-whatsapp`, `moltis-msteams`
- Automation and integrations: `moltis-browser`, `moltis-mcp`, `moltis-skills`, `moltis-caldav`, `moltis-cron`
- Security and operations: `moltis-auth`, `moltis-oauth`, `moltis-vault`, `moltis-tls`, `moltis-network-filter`, `moltis-tailscale`
- Setup and native surfaces: `moltis-provider-setup`, `moltis-openclaw-import`, `moltis-swift-bridge`, `moltis-projects`, `moltis-media`, `moltis-schema-export`, `benchmarks`

Use `--no-default-features --features lightweight` for constrained devices (Raspberry Pi, etc.).

## Security

- **Zero `unsafe` code\*** — denied workspace-wide; only opt-in FFI behind `local-embeddings` flag
- **Sandboxed execution** — Docker + Apple Container, per-session isolation
- **Secret handling** — `secrecy::Secret`, zeroed on drop, redacted from tool output
- **Authentication** — password + passkey (WebAuthn), rate-limited, per-IP throttle
- **SSRF protection** — DNS-resolved, blocks loopback/private/link-local
- **Origin validation** — rejects cross-origin WebSocket upgrades
- **Hook gating** — `BeforeToolCall` hooks can inspect/block any tool invocation

See [Security Architecture](https://docs.moltis.org/security.html) for details.

## Features

- **AI Gateway** — Multi-provider LLM support (OpenAI Codex, GitHub Copilot, Local), streaming responses, agent loop with sub-agent delegation, parallel tool execution
- **Communication** — Web UI, Telegram, Microsoft Teams, Discord, API access, voice I/O, mobile PWA with push notifications
- **Memory & Context** — Per-agent memory workspaces, embeddings-powered long-term memory, hybrid vector + full-text search, session persistence with auto-compaction, project context
- **Extensibility** — MCP servers (stdio + HTTP/SSE), skill system, lifecycle hooks with circuit breaker, destructive command guard
- **Security** — Encryption-at-rest vault (XChaCha20-Poly1305 + Argon2id), password + passkey + API key auth, sandbox isolation, SSRF/CSWSH protection
- **Operations** — Cron scheduling, OpenTelemetry tracing, Prometheus metrics, cloud deploy (Fly.io, DigitalOcean), Tailscale integration

## How It Works

Moltis is a **local-first AI gateway** — a single Rust binary that sits
between you and multiple LLM providers. Everything runs on your machine; no
cloud relay required.

```
┌─────────────┐  ┌─────────────┐  ┌─────────────┐
│   Web UI    │  │  Telegram   │  │  Discord    │
└──────┬──────┘  └──────┬──────┘  └──────┬──────┘
       │                │                │
       └────────┬───────┴────────┬───────┘
                │   WebSocket    │
                ▼                ▼
        ┌─────────────────────────────────┐
        │          Gateway Server         │
        │   (Axum · HTTP · WS · Auth)     │
        ├─────────────────────────────────┤
        │        Chat Service             │
        │  ┌───────────┐ ┌─────────────┐  │
        │  │   Agent   │ │    Tool     │  │
        │  │   Runner  │◄┤   Registry  │  │
        │  └─────┬─────┘ └─────────────┘  │
        │        │                        │
        │  ┌─────▼─────────────────────┐  │
        │  │    Provider Registry      │  │
        │  │  Multiple providers       │  │
        │  │  (Codex · Copilot · Local)│  │
        │  └───────────────────────────┘  │
        ├─────────────────────────────────┤
        │  Sessions  │ Memory  │  Hooks   │
        │  (JSONL)   │ (SQLite)│ (events) │
        └─────────────────────────────────┘
                       │
               ┌───────▼───────┐
               │    Sandbox    │
               │ Docker/Apple  │
               │  Container    │
               └───────────────┘
```

See [Quickstart](https://docs.moltis.org/quickstart.html) for gateway startup, message flow, sessions, and memory details.

## Getting Started

### Build & Run

```bash
git clone https://github.com/moltis-org/moltis.git
cd moltis
cargo build --release
cargo run --release
```

Open `https://moltis.localhost:3000`. On first run, a setup code is printed to
the terminal — enter it in the web UI to set your password or register a passkey.

Optional flags: `--config-dir /path/to/config --data-dir /path/to/data`

### Docker

```bash
# Docker / OrbStack
docker run -d \
  --name moltis \
  -p 13131:13131 \
  -p 13132:13132 \
  -p 1455:1455 \
  -v moltis-config:/home/moltis/.config/moltis \
  -v moltis-data:/home/moltis/.moltis \
  -v /var/run/docker.sock:/var/run/docker.sock \
  ghcr.io/moltis-org/moltis:latest
```

Open `https://localhost:13131` and complete the setup. See [Docker docs](https://docs.moltis.org/docker.html) for Podman, OrbStack, TLS trust, and persistence details.

### Cloud Deployment

| Provider | Deploy |
|----------|--------|
| DigitalOcean | [![Deploy to DO](https://www.deploytodo.com/do-btn-blue.svg)](https://cloud.digitalocean.com/apps/new?repo=https://github.com/moltis-org/moltis/tree/main) |

**Fly.io** (CLI):

```bash
fly launch --image ghcr.io/moltis-org/moltis:latest
fly secrets set MOLTIS_PASSWORD="your-password"
```

All cloud configs use `--no-tls` because the provider handles TLS termination.
See [Cloud Deploy docs](https://docs.moltis.org/cloud-deploy.html) for details.

## Star History

[![Star History Chart](https://api.star-history.com/svg?repos=moltis-org/moltis&type=date&legend=top-left)](https://www.star-history.com/#moltis-org/moltis&type=date&legend=top-left)

## License

MIT
