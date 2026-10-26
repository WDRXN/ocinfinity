# Changelog

## 1.0.0 — first release

`ocinfinity` starts here as a single 1.0.0 line. Earlier 2.x builds were the
pre-rebrand prototype (`oplire`); their history is intentionally not carried
forward, so this entry describes the whole tool.

### Features

- **WARP rate-limit reset.** `quick-reset` registers a fresh Cloudflare WARP
  identity and verifies the tunnel is back up; `reset` additionally cycles the
  service. The proxy rotates on its own when the upstream answers `429`.
- **Anthropic ⇄ OpenAI bridge.** Claude Code talks to `127.0.0.1:8080` and
  `ocinfinity` translates to the OpenCode gateway, including SSE streaming and
  tool calls, in both directions.
- **OpenCode v2 discovery.** The v2 background service binds a new port on every
  start and speaks `/api/*` only, so it can never be the bridge target. The
  upstream is read from it the way the v2 client does: `opencode service status`,
  then `GET /api/provider`, yielding `https://opencode.ai/inference/v1`.
- **Zero configuration.** Upstream, listen port, WARP, OpenCode and Claude Code
  are all detected at runtime; a running instance is reused instead of
  duplicated, recognised by the `x-ocinfinity` header.
- **Self-diagnosis.** `doctor`, `detect`, `status` and `models` report what was
  found and what is reachable, with `--json` for scripting.
- **Installers.** `install.sh` and `install.ps1` fetch the right release asset,
  verify it against `SHA256SUMS` and put it on `PATH`.

### Notes

- Every outbound request sends a `User-Agent`. Cloudflare fronts the gateway and
  rejects requests without one, which surfaces as empty-bodied `400`/`403`.
- The free tier is metered per account. When it is exhausted the gateway answers
  `FreeUsageLimitError` (and so does the OpenCode client itself); the bridge
  reports that as an Anthropic-shaped `429`.
- Linux builds link against glibc and use rustls, so no OpenSSL is required at
  build time or runtime.
