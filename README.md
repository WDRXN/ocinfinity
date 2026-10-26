```
   ┌──────────────────────────────────────────────────┐
   │   O C I N F I N I T Y                            │
   │   OpenCode limit reset · Claude Code bridge      │
   └──────────────────────────────────────────────────┘
```

[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange?style=flat-square&logo=rust)](https://www.rust-lang.org)
[![Version](https://img.shields.io/badge/version-1.0.0-blue?style=flat-square)](Cargo.toml)
[![License](https://img.shields.io/badge/license-proprietary-red?style=flat-square)](LICENSE)

## What is ocinfinity?

**ocinfinity** is a dual-purpose tool:

1. **WARP Rate Limit Reset** — rotates your IP through Cloudflare WARP to reset
   OpenCode rate limits.
2. **Claude Code Proxy Bridge** — a reverse proxy that connects Claude Code to
   OpenCode Zen's free models with automatic rate-limit recovery.

It configures itself. Ports, endpoints and installed tools are discovered at
runtime, so nothing has to be wired by hand.

### How It Works

#### WARP reset mode

OpenCode tracks usage by IP. When you hit the rate limit, `ocinfinity`:

1. **Stops** the current WARP tunnel
2. **Clears** the cached registration
3. **Creates** a new tunnel registration (new IP)
4. **Restarts** WARP with the fresh IP

#### Proxy bridge mode

```
Claude Code  →  ocinfinity proxy (127.0.0.1:8080)  →  OpenCode Zen
```

- Translates the Anthropic API format to the OpenAI format and back
- Streams SSE responses in real time, including tool calls
- Exposes the live model list through `/v1/models`
- **Auto-resets WARP** on `429` responses, transparently

#### Automatic discovery

`ocinfinity` never assumes a fixed layout:

| What | How it is resolved |
|------|--------------------|
| OpenCode upstream | `--upstream` → saved config (if it answers) → OpenCode v2 service provider → live port scan → `https://opencode.ai/inference/v1` |
| Proxy listen port | `--listen` → next free port from `8080` upwards |
| Running instance | Detected via the `x-ocinfinity` response header; reused instead of duplicated |
| Claude Code | Located on `PATH`, including npm `.cmd` shims on Windows |
| WARP | Located on `PATH` or at the platform default install location |

Run `ocinfinity detect` at any time to see what was found.

### OpenCode v2

OpenCode v2 changed shape. Its local background service now listens on a
**different port every start** and speaks `/api/*` (sessions, models, events)
rather than an OpenAI-compatible chat API, so scanning for a fixed port is not
enough and talking to the service directly is not possible.

`ocinfinity` handles v2 the same way the v2 client does:

1. `opencode service status` reports the current service URL.
2. `GET /api/provider` on that service (authenticated with the password in
   `~/.config/opencode/service.json`) returns the provider v2 uses for
   inference.
3. That provider is `https://opencode.ai/inference/v1` — plain
   OpenAI-compatible, and the upstream the bridge forwards to.

Nothing has to be configured. When the service is stopped, the known gateway is
used directly; v1 installs and custom OpenAI-compatible servers are still found
by port scanning.

Two details matter for reliability:

- Every request carries a `User-Agent`. Cloudflare fronts the gateway and
  rejects requests without one.
- Model listing is read from the base that serves `/models`, while chat uses the
  same base — the version suffix is never appended twice.

## Install

One line. No toolchain, no configuration.

```bash
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/WDRXN/ocinfinity/main/install.sh | sh
```

```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/WDRXN/ocinfinity/main/install.ps1 | iex
```

```bash
# macOS, via Homebrew
brew install WDRXN/ocinfinity/ocinfinity
```

The installers detect the platform, download the matching release asset, verify
it against the published `SHA256SUMS`, install it and put it on `PATH`. Pin a
version with `OCNINFINITY_VERSION=v1.0.0`, choose a directory with
`OCNINFINITY_INSTALL_DIR`, and remove it again with `install.sh --uninstall` or
`$env:OCNINFINITY_UNINSTALL=1`.

Where the binaries land:

| Platform | Path |
|----------|------|
| Linux / macOS | `/usr/local/bin` when writable, otherwise `~/.local/bin` |
| Windows | `%LOCALAPPDATA%\Programs\ocinfinity` (added to the user `PATH`) |

### Manual download

Every release ships `ocinfinity-windows.exe`, `ocinfinity-linux`,
`ocinfinity-macos` (Intel), `ocinfinity-macos-arm` (Apple Silicon) and
`SHA256SUMS`:

<https://github.com/WDRXN/ocinfinity/releases/latest>

```bash
curl -fsSLO https://github.com/WDRXN/ocinfinity/releases/latest/download/SHA256SUMS
sha256sum -c SHA256SUMS --ignore-missing
```

The Linux build links against glibc (`libc`, `libm`, `libgcc_s`) and uses
rustls, so no OpenSSL is needed at build time or runtime.

### From source
```bash
git clone https://github.com/WDRXN/ocinfinity.git
cd ocinfinity
cargo build --release
sudo cp target/release/ocinfinity /usr/local/bin/
```

### Package manifests

`winget/ocinfinity.yaml`, `aur/PKGBUILD` and `homebrew/ocinfinity.rb` are kept in
tree. The Homebrew tap is published, so `brew install` works today; the winget
and AUR manifests take effect once they are accepted upstream.

### Guided setup
```bash
ocinfinity setup
```

Installs Cloudflare WARP, OpenCode v2 (`@opencode/cli`) and the latest Claude
Code (`@anthropic-ai/claude-code@latest`) as needed.

## Usage

### Quick start — Claude Code + proxy
```bash
# One command: detects the upstream, starts the proxy, launches Claude Code
ocinfinity connect claude-code

# Pin a model instead of choosing interactively
ocinfinity connect claude-code --model glm-4.7-free

# Point at a specific OpenCode Zen instance
ocinfinity connect claude-code --upstream http://my-opencode-server:3000

# Pass arguments straight through to Claude Code
ocinfinity connect claude-code -- --permission-mode acceptEdits
```

### WARP reset commands
```bash
ocinfinity reset          # Full WARP tunnel reset
ocinfinity quick-reset    # Fast IP rotation (no service restart)
ocinfinity status         # Check WARP connection status
ocinfinity status --json  # Machine-readable status
ocinfinity stop           # Stop WARP tunnel
ocinfinity install warp   # Install Cloudflare WARP
```

### Proxy commands
```bash
ocinfinity proxy                          # Reverse proxy on the first free port from 8080
ocinfinity proxy --listen 0.0.0.0:9000    # Custom listen address
ocinfinity proxy --upstream http://host:4096
ocinfinity daemon                         # Background daemon mode
ocinfinity watch                          # Monitor OpenCode, auto-reset on 429
```

### Discovery and diagnostics
```bash
ocinfinity detect         # Detected services, chosen upstream, next free port
ocinfinity detect --json  # Same, as JSON
ocinfinity doctor         # Full environment check
ocinfinity models         # List the models the upstream exposes
ocinfinity about          # Version and command reference
```

### Configuration
```bash
ocinfinity config show    # Show current settings
ocinfinity config set --upstream http://localhost:4096 --listen 127.0.0.1:9000
ocinfinity config reset   # Reset to defaults
```

Saved values are advisory, not binding: if a saved upstream stops answering,
discovery takes over on the next run.

## Claude Code integration

### Method 1: one command (recommended)
```bash
ocinfinity connect claude-code
```

### Method 2: manual environment
```bash
# Start the proxy in the background
ocinfinity daemon &

# Wire Claude Code to it
export ANTHROPIC_BASE_URL=http://127.0.0.1:8080
export ANTHROPIC_API_KEY=ocinfinity-proxy-key
claude
```

### Method 3: Claude Code settings
```bash
claude
> /config
# API Base URL: http://127.0.0.1:8080
# API Key:      ocinfinity-proxy-key
```

The proxy does not authenticate clients. The key above is a placeholder so the
upstream credential never leaves `ocinfinity`.

## Available free models

When the upstream is unreachable, the bridge serves this fallback catalogue so
Claude Code still has something to select:

| Model | ID |
|-------|-----|
| Big Pickle | `big-pickle` |
| GLM 5.3 Flash Free | `glm-5.3-flash-free` |
| DeepSeek V4.1 Flash Free | `deepseek-v4.1-flash-free` |
| MiMo V2.6 Flash Free | `mimo-v2.6-flash-free` |
| Nemotron 3.5 Lightning Free | `nemotron-3.5-lightning-free` |
| Nemotron 3.5 Super VL Free | `nemotron-3.5-super-vl-free` |
| Nemotron 3 Ultra Free | `nemotron-3-ultra-free` |
| Beam Free | `beam-free` |
| Flint 40B Free | `flint-40b-free` |
| Jev 1.13 Free | `jev-1.13-free` |

The gateway also serves paid models; when it is reachable, `/v1/models` returns
its full live list instead of this fallback.

## Global options

- `--verbose` — debug logging on stderr
- `--dry-run` — print the actions instead of performing them
- `--json` — machine-readable output (`status`, `detect`)
- `--help` — command reference
- `--version` — version string

## About

```
Version:        1.0.0
Language:       Rust
Purpose:        OpenCode rate limit reset + Claude Code proxy
Infrastructure: Cloudflare WARP + Axum HTTP
Author:         WDRXN
GitHub:         https://github.com/WDRXN/ocinfinity
```

## License

Proprietary, source-available, all rights reserved. Viewing and evaluation are
permitted; use, modification, redistribution and commercial exploitation are
not. See [LICENSE](LICENSE) for the exact terms.

---

© 2026 WDRXN
