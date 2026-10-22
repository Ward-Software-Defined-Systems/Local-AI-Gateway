# WSDS Local AI Gateway

A monitoring, inspection and data-loss-prevention reverse proxy for local LLM servers, with a web dashboard. It sits in front of **LM Studio** and **Ollama**, records every request's tokens, time to first token and throughput, prices them at frontier-API list rates, keeps a searchable copy of each request for debugging, scans prompts and replies for secrets and personal data, alerts you, and shows hardware telemetry, all from one binary.

It replaces the two terminal monitors it grew out of, [LMS-Monitor](https://github.com/Ward-Software-Defined-Systems/LMS-Monitor) and [Ollama-Monitor](https://github.com/Ward-Software-Defined-Systems/Ollama-Monitor).

## Features

- **Reverse proxy for both backends.** One listener per backend, byte-transparent: clients see exactly what LM Studio or Ollama sent. Streams are teed, never buffered. WebSocket clients (the LM Studio SDK) are tunnelled.
- **Every API format parsed.** OpenAI chat, completions, embeddings and Responses; Anthropic Messages; Ollama native chat, generate and embeddings; LM Studio native `/api/v0`. Tokens, time to first token, tokens per second and stop reasons per request; estimates are marked `~` when an engine gave no counts.
- **Overview, LM Studio and Ollama tabs**: the live feed, loaded and cloud models, hardware (CPU, memory, GPU, and ANE power on macOS or VRAM on Linux), rolling 1m/5m/15m/session windows, hypothetical frontier cost, charts.
- **Requests inspector**: keyset-paginated history with filters and full-text search over bodies, each request with its request and response bodies, reassembled reply text, headers, chunk timeline, findings, copy-as-curl and download.
- **DLP**: 38 built-in rules (cloud keys, tokens, private keys, JWTs, database URLs, payment cards, IBANs, social security numbers, emails, phones) plus your own regexes and exact known secrets. Findings carry a fingerprint and a masked preview, never the value. Stored copies are redacted. Requests to Ollama cloud models carrying a secret are **blocked** before they leave the machine.
- **Alerts**: in-app alert centre, desktop notifications (macOS and Linux desktops) and a webhook, throttled per rule so one noisy client cannot flood you. Backend outages and recoveries alert too.
- **Hot reload**: DLP rules, policy, alerts, capture limits and prices change without a restart, from the Settings tab or with SIGHUP.
- **Runs as a service**: a launchd agent on macOS, a systemd user unit on Linux, installed with one command.

## Platforms

macOS (Apple Silicon) and Ubuntu Linux (x86_64 with an NVIDIA GPU) are both supported and tested. GPU telemetry comes from `ioreg` on macOS and `nvidia-smi` on Linux; desktop notifications use `osascript` or `notify-send`; the service runs under launchd or systemd. The test suite needs no GPU, LM Studio or Ollama and runs on Linux in CI.

## Quick start

Requirements: Rust 1.97 (see `rust-toolchain.toml`) and Node 24 for the dashboard build.

```sh
make release                                  # builds the dashboard, then the binary with it embedded
install -m 755 target/release/local-ai-gateway ~/.local/bin/
local-ai-gateway config init                  # writes a commented config.toml with every default
local-ai-gateway                              # prints the dashboard URL with its access token
```

Open the printed URL once; it sets a cookie and redirects to the clean address. Later, `local-ai-gateway --print-token` prints it again.

To run it at login:

```sh
local-ai-gateway service install              # launchd agent (macOS) or systemd user unit (Linux)
local-ai-gateway service status
local-ai-gateway setup check                  # configuration, file modes, backends, listeners, tools
```

On a headless Linux server run `loginctl enable-linger $USER` once so the user unit outlives your login session. Set `[dashboard] listen = "0.0.0.0:8787"` there; the dashboard then serves HTTPS with a self-signed certificate.

### Pointing clients at the gateway

The backends keep their own ports; the gateway listens on its own (defaults `127.0.0.1:1235` for LM Studio and `127.0.0.1:11435` for Ollama; set `listen` under `[backends.<name>]` to taste, `0.0.0.0` to serve other machines). Anything that still talks to a backend directly is invisible to the gateway.

| Client | Setting |
|---|---|
| OpenAI-compatible SDKs and tools | `OPENAI_BASE_URL=http://<host>:11435/v1` (Ollama) or `http://<host>:1235/v1` (LM Studio) |
| Ollama CLI and native clients | `OLLAMA_HOST=<host>:11435` |
| Anthropic-format clients | `ANTHROPIC_BASE_URL=http://<host>:11435` or `http://<host>:1235` |
| LM Studio SDK | `new LMStudioClient({ baseUrl: "ws://<host>:1235" })` |

The `lms` CLI talks to LM Studio's internal port and is unaffected.

## The dashboard

| Tab | What it shows |
|---|---|
| Overview | both backends: feed, models, hardware, rolling metrics, cost, charts |
| LM Studio, Ollama | the same, for one backend |
| Requests | the history with filters and search; a request's bodies, headers, timeline and findings |
| Security | findings and alerts of the last 24 hours, the alert centre, every rule with its effective policy, a test box, the cloud-egress counter |
| Settings | runtime facts and paths, the `config.toml` editor (check, save and apply, reload), maintenance |

The status bar shows backend reachability and versions, the live-socket state, unacknowledged alerts, cloud requests in the last 24 hours and the lifetime totals. The feed can be paused (`PAUSED`) and the session metrics reset, as in the terminal monitors.

## Configuration

`config.toml` lives in the data directory: `~/Library/Application Support/local-ai-gateway/` on macOS, `~/.config/local-ai-gateway/` on Linux (data under `~/.local/share/local-ai-gateway/`). `--data-dir` moves everything, `--config` just the file; `LAIG_CONFIG`, `LAIG_DATA_DIR`, `LAIG_DASHBOARD_LISTEN`, `LAIG_DASHBOARD_TOKEN` and `LAIG_LOG` stand in for the flags. Every key is optional and unknown keys are errors. `local-ai-gateway config init` writes a commented file with every default; `--check-config` validates one.

| Section | Keys | Reload |
|---|---|---|
| `[dashboard]` | `listen`, `auth`, `tls` (`auto`, `on`, `off`), `token_file` | restart |
| `[backends.lmstudio]`, `[backends.ollama]` | `enabled`, `listen` (list), `upstream`, `capture_bodies`, `record_passthrough`, `rewrite_include_usage`, `poll_interval_secs`; LM Studio: `lms_bin`, `app_path` | restart |
| `[capture]` | `max_body_bytes`, `retention_days`, `passthrough_retention_hours`, `hardware_raw_retention_hours`, `max_buffered_request_bytes` | live |
| `[dlp]`, `[dlp.cloud]`, `[dlp.rules.<id>]`, `[dlp.backends.<name>]`, `[[dlp.custom_rules]]`, `[[dlp.allowlist]]` | see below | live |
| `[alerts]`, `[alerts.desktop_notification]`, `[alerts.webhook]` | `throttle_secs`; `enabled`, `min_severity`; `url`, `headers` or `headers_file`, `timeout_secs` | live |
| `[pricing.providers.<p>.models.<key>]` | `input_per_mtok_usd`, `output_per_mtok_usd` | live |
| `[hardware]` | `enabled`, `sample_interval_secs`, `gpu_source`, `powermetrics` | restart |
| `[log]` | `level`, `max_files` | restart |

"Live" sections apply when you save in the Settings tab, call `POST /api/config/reload`, or send `SIGHUP`. The Settings tab shows which sections still need a restart.

`rewrite_include_usage` adds `stream_options.include_usage` to streaming chat and completion requests (`/v1/chat/completions`, `/v1/completions` and LM Studio's `/api/v0/chat/completions`). LM Studio sends no token counts on a stream without it; Ollama sends them either way. It is the only change the gateway makes to a request.

## DLP

Every inference request body is scanned before it is forwarded: JSON bodies leaf by leaf (a finding says `$.messages[3].content`), anything else as text. The reply text is scanned when the response ends. A finding records the rule, severity, location, a 32-character fingerprint and a masked preview (`AKIA…MPLE`); the plaintext is never stored, logged or shown. Stored copies replace each value with `[REDACTED:<rule>:<fingerprint>]`, so a redacted JSON body stays valid JSON.

**Actions.** A request-side finding is `record` (kept in the Security tab), `alert` (also notified) or `block` (refused with 403 and a `x-gateway-blocked: dlp` header; the backend never sees it). Replies can be `record` or `alert`. The order of precedence is the rule's own default, `[dlp] default_action`, `[dlp.backends.<name>]`, `[dlp.rules.<id>]`, then the cloud override: a request routed to an Ollama cloud model takes the stricter of its action and `[dlp.cloud] request_action` (`block` by default; `pii_action` for personal data). Secret rules default to the global action; PII rules record (email, phone) or alert (payment card, IBAN, social security number).

**Rules.** The 38 built-in rules cover AWS, GitHub, GitLab, Slack, Stripe, Google, GCP service accounts, Anthropic, OpenAI, Hugging Face, npm, PyPI, Twilio, SendGrid, Mailgun, Azure storage, DigitalOcean and Telegram credentials, PEM and OpenSSH private keys, age keys, JWTs, generic `password =` style assignments with high entropy, database and basic-auth URLs with embedded passwords, bearer tokens, and the PII rules. Each has a positive and a negative test vector. Add your own with `[[dlp.custom_rules]]` (regex crate syntax, optional capture group, keywords and entropy gate); disable, re-severity or re-action a rule with `[dlp.rules.<id>]`; exempt values with `[[dlp.allowlist]]` by rule, regex, fingerprint, direction or backend. Local dummy keys such as `sk-lm-studio` or `ollama` are allowlisted out of the box.

**Known secrets.** Put exact values in `known-secrets.toml` in the data directory (mode 0600):

```toml
[[secret]]
kind = "vault-root-token"
value = "hvs.…"
```

They are matched raw, percent-encoded, base64 (standard and URL-safe) and as a bare `Bearer` token, and reported by kind. The file is read at start and on every reload.

**Test box.** The Security tab scans pasted text under the live policy and shows what would be found, whether it would be blocked, and what the stored copy would look like. Nothing entered there is stored or logged.

## Alerts

A request's findings become one alert when they reach `notify_min_severity` (`high` by default) or the request was blocked. Backends that fail three polls in a row raise an alert; recovery raises another. Every alert goes to the in-app list; desktop notifications (`osascript` on macOS, `notify-send` on a Linux desktop) and the webhook (a JSON `POST` with `kind`, `severity`, `title`, `detail`, `request_id`, `created_at`, plus your headers) are throttled to one per `throttle_secs` per rule, kind and backend, with the suppressed count attached to the next one. Notification text carries masked fields only. Webhook header values can live in a separate `headers_file` (mode 0600) so they never appear in the config editor.

## API

Every dashboard endpoint is under `/api`, JSON, gated by the token cookie (or `?token=`). Errors are `{"error": {"code", "message"}}`.

| Endpoint | Purpose |
|---|---|
| `GET /api/status` | version, platform, session, listeners, backends, database, DLP summary, paths |
| `GET /api/models?backend=` | the unified model list |
| `GET /api/hardware/current`, `GET /api/hardware/series?from&to` | telemetry |
| `GET /api/metrics/rolling`, `/series?metric=`, `/cost`, `/lifetime` | metrics |
| `GET /api/requests?cursor&limit&backend&model&outcome&format&client&cloud&has_findings&from&to&q&include_passthrough` | history (`q` is full-text search) |
| `GET /api/requests/inflight`, `GET /api/requests/{id}`, `GET /api/requests/{id}/body/{request\|response}`, `DELETE /api/requests/{id}/bodies` | one request |
| `GET /api/findings?rule&severity&backend&direction&from&to&before&limit` | findings |
| `GET /api/alerts?unacked&before&limit`, `POST /api/alerts/{id}/ack`, `POST /api/alerts/ack-all` | alerts |
| `GET /api/dlp/rules`, `POST /api/dlp/test {text, backend, is_cloud}`, `POST /api/dlp/reload` | DLP |
| `GET /api/config`, `GET /api/config/file`, `POST /api/config/validate {text}`, `PUT /api/config {text}`, `POST /api/config/reload` | configuration |
| `POST /api/session/reset`, `POST /api/retention/run`, `GET /api/sessions` | housekeeping |
| `GET /ws` | the event stream: `hello`, then every event (`request_started`, `request_completed`, `finding`, `alert`, `backend_snapshot`, `hardware`, `metrics`, `config_changed`, `shutdown`) |

## Files

| File | Purpose |
|---|---|
| `config.toml` | configuration (optional) |
| `gateway.db` (+ `-wal`, `-shm`) | SQLite: requests, bodies (full-text indexed), findings, alerts, hardware samples, rollups |
| `dashboard.token` | the access token, mode 0600 |
| `tls/cert.pem`, `tls/key.pem` | the self-signed certificate when TLS is on |
| `known-secrets.toml` | exact values to watch for, mode 0600 |
| `logs/local-ai-gateway.YYYY-MM-DD.log` | the daily log; `logs/service.*.log` under launchd |

Request metadata and findings are kept indefinitely; bodies expire after `retention_days` (7), passthrough rows after 24 hours, raw hardware samples after 48 hours, acknowledged alerts after 90 days.

## Project structure

| Path | Role |
|---|---|
| `crates/gateway-core` | types, configuration, paths, event bus, rolling aggregation, pricing, the SQLite store |
| `crates/gateway-proxy` | listeners, route classifier, request and response pipelines, stream parsers, WebSocket tunnel |
| `crates/gateway-dlp` | rules, known secrets, scanning, redaction, policy |
| `crates/gateway-backends` | LM Studio and Ollama clients and pollers |
| `crates/gateway-hardware` | sysinfo, ioreg, nvidia-smi, powermetrics |
| `crates/local-ai-gateway` | the binary: CLI, wiring, hot reload, alerts, web layer, service and setup commands |
| `frontend/` | the dashboard (Vite, React, TypeScript, Tailwind), embedded into the binary at build time |
| `fixtures/` | real captures the parsers are tested against |

`make test` runs what CI runs: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, and the frontend typecheck and lint.

## Security notes

- The dashboard binds to loopback by default and requires the access token; `auth = false` is only accepted on loopback. Off loopback, `tls = "auto"` serves HTTPS with a self-signed certificate.
- Proxy listeners carry no authentication, like the backends behind them. Bind them to loopback unless other machines need them, and consider setting LM Studio's network interface to `127.0.0.1` so nothing bypasses the gateway.
- No request or response body, header value or access token is ever written to the log. Bodies reach only the database, DLP-redacted; `authorization`, `x-api-key`, `cookie` and `proxy-authorization` header values are redacted in stored copies.
- The database is not encrypted. It holds prompts; treat the data directory accordingly.
- Blocking happens on the request side only. A secret in a model's reply is recorded and alerted, never cut from the stream.

## Moving over from the monitors

1. Stop LMS-Monitor and Ollama-Monitor (the latter held port 11435, which the gateway's Ollama listener takes over by default).
2. Set `listen` and `upstream` for each backend in `config.toml`, then `local-ai-gateway --check-config`.
3. `local-ai-gateway service install`, then `setup check`.
4. Repoint clients per the table above and send one request per client type; confirm the rows in the Requests tab.
5. Keep the monitors' `usage.db` files; importing them is on the roadmap.

## Known issues

- LM Studio reports zero prompt tokens for embeddings (`usage.prompt_tokens: 0`), so embedding requests on that backend show no prompt count.
- LM Studio's native streaming endpoint carries no engine `stats` (tokens per second, time to first token); those arrive on non-streaming `/api/v0` requests only. Streams get the gateway's wall-clock figures, marked `~`.
- A context-overflow rejection from LM Studio (HTTP 400) is parsed from the wordings the terminal monitors recorded; a fresh capture is still wanted.
- An error inside a 2xx stream is recorded as `failed` with the 200 status the client already received.
- Open streams that outlive the five-second shutdown grace are cut without a record.
- Reply text is scanned once at the end of the response, not incrementally.
- Changing a backend's `capture_bodies`, `record_passthrough` or `rewrite_include_usage` needs a restart.

## Roadmap

A unified `/v1` endpoint with model routing and aliases · per-client API keys and policy · import of the monitors' `usage.db` histories · model management (load, unload, pull, keep-alive countdown) · Prometheus `/metrics` · bypass detection via `lms log stream` · stopping a reply stream on a finding · encrypted database · Intel and AMD GPU telemetry on Linux.

## Licence

MIT, © Ward Software Defined Systems LLC.
