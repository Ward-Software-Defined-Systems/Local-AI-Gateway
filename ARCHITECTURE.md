# Architecture

Maintainer notes. The README is the user guide. This file is updated whenever behaviour changes and
keeps a "Known gaps" list at the end.

## Platforms

macOS (Apple Silicon) and Ubuntu Linux (x86_64 with NVIDIA) are first-class targets; the original
LMS-Monitor and Ollama-Monitor run on both and so does this gateway. Platform-specific code sits behind
`cfg(target_os)` or a `Platform::current()` match: GPU telemetry (`ioreg` and optional `powermetrics` on
macOS, `nvidia-smi` on Linux), the desktop-notification sink (`osascript` / `notify-send`), the service
manager (launchd agent / systemd user unit) and the data directories (Application Support / XDG). Both
telemetry parsers and both unit generators compile and test on every platform; only the live samplers and
the process calls are gated. CI runs the full suite on Linux.

## Process model

One binary, one process. `main` resolves paths, loads and validates `config.toml` (including a compile of
the DLP rules), starts tracing (a daily-rotated file plus stderr when interactive), binds every listening
socket before the runtime exists so a taken port fails fast, then runs the tasks on a multi-thread tokio
runtime:

| Task | Role |
|---|---|
| dashboard server | axum-server over HTTP or TLS: token gate, embedded SPA, REST, `/ws` |
| signal task | SIGINT/SIGTERM stop; SIGHUP reloads `config.toml` and the known-secrets file through `Hot` |
| proxy listeners | one axum server per configured address, forwarding to one backend (`gateway-proxy`) |
| store writer | the only writing SQLite connection; batches commands into transactions (`gateway-core::store`) |
| retention | deletes bodies, passthrough rows and raw hardware samples past their windows, every 10 minutes; reads the policy from `Hot` each run |
| backend pollers | LM Studio and Ollama model lists and reachability, every 2 s (`gateway-backends`) |
| hardware sampler | sysinfo plus ioreg or nvidia-smi, every 2 s (`gateway-hardware`) |
| metrics loop | rolling windows from completed requests, republished every 2 s, one log line per feed row |
| alert dispatcher | turns findings, blocks and backend outages into alerts; stores, publishes and delivers them |

Shutdown gives open connections five seconds, under launchd's ten and systemd's fifteen.

### Hot reload

`crates/local-ai-gateway/src/hot.rs` holds what changes without a restart: the current `Config`, the DLP
`Engine`, the pricing table, the retention policy and the alert settings. `Hot::apply` validates a whole
configuration (structure, pricing, DLP compile, known-secrets file), swaps the engine into the proxy's
scanner slot, the capture policy and pricing into the proxy context, the alert settings and the retention
policy, then publishes `config_changed` with the sections that differ from the boot configuration and only
take effect on restart (`dashboard`, `backends.<name>`, `hardware`, `log`). CLI overrides (`--dashboard-listen`,
`--log-level`) are re-applied to every reload so a reload of the unchanged file is a no-op. `PUT /api/config`
writes the file atomically (write, rename) and applies; `POST /api/config/reload` and SIGHUP re-read it.
Errors leave everything as it was.

## Workspace

| Crate | Role |
|---|---|
| `gateway-core` | shared types, `config.toml` schema and validation, paths, event bus, aggregation, pricing, store |
| `gateway-proxy` | listeners, classifier, request/response pipelines, parsers, upgrade tunnel, capture, the `RequestScanner` hook |
| `gateway-dlp` | rules, known-secret matcher, engine (JSON walk, raw scan, allowlist, fingerprints, previews), redaction, policy |
| `gateway-backends` | LM Studio and Ollama clients and pollers |
| `gateway-hardware` | sysinfo, ioreg, nvidia-smi, powermetrics |
| `local-ai-gateway` | the binary: CLI, wiring, hot reload, alert dispatcher, web layer, `service` and `setup` |

`frontend/` is a Vite + React + TypeScript + Tailwind app. `crates/local-ai-gateway/build.rs` stages
`frontend/dist` into `OUT_DIR/ui` (or a placeholder page when the frontend is not built) and rust-embed
compiles it in; it emits `rerun-if-changed` for every file under `dist/` so a frontend-only rebuild
re-embeds.

## DLP engine

`gateway-dlp` is pure: no I/O, every rule has a millisecond test. A `Rule` is a regex (crate syntax, no
look-around), the capture group holding the secret, optional lowercase keywords that must occur in the text
before the regex runs, an optional Shannon-entropy floor, and an optional validator (Luhn, IBAN mod-97, JWT
header). The built-in rules plus `[[dlp.custom_rules]]` compile into one `RegexSet` for the first pass;
only rules that hit run `captures_iter`. Disabled rules are compiled as `a^` so the set never matches them.
Custom rule ids may not shadow built-in ids (override those under `[dlp.rules.<id>]`).

`Engine::scan_text` returns every match with overlaps collapsed to the most severe, then longest, span.
`scan_request` parses JSON bodies and scans each string leaf so findings carry a JSON path and the stored
copy is re-serialised with the leaves redacted (the placeholder `[REDACTED:<rule>:<fp8>]` holds no quotes or
backslashes, so the copy stays valid JSON); non-JSON bodies are scanned raw with byte offsets. Bodies past
`max_scan_bytes` are scanned up to the cap and flagged. `scan_response` scans the reassembled reply text,
replaces each value in the raw stream copy where it occurs whole, and falls back to the redacted reply text
when a value was split across chunks, so no secret is ever stored.

Known secrets (`known-secrets.toml`, read by the binary with a mode check) are matched in every form they
travel in: raw, percent-encoded, base64 standard and URL-safe, padded or not, and the bare token of a
`Bearer` value; overlapping hits collapse to the longest. Findings carry `fingerprint` (the first 32 hex
characters of the SHA-256 of the value) and `preview` (`AKIA…MPLE` for twelve characters or more, `****`
below, the BEGIN/END markers for PEM blocks). The allowlist (rule, regex on the value, fingerprint prefix,
direction, backend) removes matches before policy; a built-in entry drops the dummy keys local clients send
(`sk-lm-studio`, `ollama`, `no-key`, …).

Policy resolution (`policy.rs`): the rule's default → `[dlp] default_action` / `response_action` →
`[dlp.backends.<name>]` → `[dlp.rules.<id>]` → for cloud-routed requests the stricter of the result and
`[dlp.cloud] request_action` (or `pii_action` for PII rules). Cloud only tightens. `redact` is the rule's
default or its override, and off entirely when `redact_stored_copies = false`.

The proxy sees the engine through `gateway_proxy::scan::RequestScanner` (`crates/local-ai-gateway/src/dlp.rs`
adapts it). A block answers 403 with a body shaped for the request's API family and `x-gateway-blocked: dlp`,
records `Outcome::Blocked` with the redacted body and findings, and never contacts the upstream.

## Alerts

`crates/local-ai-gateway/src/alerts.rs` subscribes to the bus. Findings are queued by request id and turned
into one alert when `request_completed` arrives, if the findings reach `notify_min_severity` or the outcome
is `blocked`. A backend unreachable for three polls in a row raises `backend_unreachable`; the first
reachable poll after that raises `backend_recovered`. Every alert is stored (`alerts` table) and published
(`alert` event) untrottled; the desktop and webhook sinks are throttled per `(kind, rule, backend)` to one
per `throttle_secs`, and the next delivered alert carries `suppressed_before_this` in its detail. Desktop
notifications run `osascript` (macOS) or `notify-send` (Linux; its absence is logged once and the sink is
off); the webhook posts JSON with the configured headers (`headers_file` merged in) and retries once. Text
reaching a sink is built from masked fields only.

## Web layer

`access.rs` gates every route on the token cookie (`laig_token`, HttpOnly, SameSite=Strict, Secure under
TLS) or `?token=` (which passes `/api` and `/ws` directly and sets the cookie with a 303 elsewhere).
`assets.rs` serves the embedded SPA with `Cache-Control: no-store` and a SPA fallback. REST handlers live
under `web/api/` (status, models, hardware, metrics, requests, alerts, dlp, config, session); `ws.rs` sends
`hello` then every bus event verbatim and tells a lagging client to `resync`. `AppState` reads the
configuration, engine and pricing through `Hot`, so a reload is visible to the next request.

## Invariants

- The proxy never changes the bytes a client receives. The only request change is the OpenAI
  `stream_options.include_usage` rewrite on streaming `/v1` requests.
- No request or response body, header value or access token is ever written to the log. Bodies reach
  only the SQLite capture store, DLP-redacted.
- A finding never carries the matched value: fingerprint and masked preview only. Nothing typed into the
  test box is stored or logged.
- Unknown `config.toml` keys are errors. Validation errors stop startup and refuse a reload; warnings are logged.
- `dashboard.auth = false` is only accepted on a loopback bind; `tls = "auto"` enables HTTPS off loopback.
- Tests need no terminal, sudo, GPU, LM Studio or Ollama; anything that touches the machine is `#[ignore]`.
- A reload either applies whole or changes nothing.

## Known gaps

- The Anthropic (`/v1/messages`), Responses (`/v1/responses`) and LM Studio native (`/api/v0`) parsers follow the
  public API shapes and synthetic samples; real captures from both backends are still to be taken as fixtures.
- Whether LM Studio needs `stream_options.include_usage` on `/v1` streams is undecided; the rewrite is on for
  both backends until the fixture pair settles it.
- An error inside a 2xx stream is recorded as `failed` with the 200 status the client already received.
- Open streams that outlive the shutdown grace are cut without a `client_aborted` record.
- Response-side DLP is a single scan of the reassembled text at the end; findings mid-stream and stopping a
  stream on a finding are not implemented.
- `backends.<name>.capture_bodies`, `record_passthrough` and `rewrite_include_usage` live in the listener
  and need a restart; the Settings tab reports them under `backends.<name>`.
- The `cloud_egress` alert kind exists but is unused; cloud requests are counted (status, Security tab) rather
  than alerted per request.
- The Requests inspector shows the raw captured stream; reply text is reassembled client-side from the
  OpenAI, Anthropic, Responses and Ollama chunk shapes, so a format the parser does not know shows raw only.
- `service` and `setup` have been exercised on macOS; the Linux arms are unit-tested (unit text, paths) but
  not yet run on the Ubuntu box.
