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
| proxy listeners | one axum server per configured address: a fixed listener forwards to one backend, the unified listener picks the backend per request (`gateway-proxy`) |
| store writer | the only writing SQLite connection; batches commands into transactions (`gateway-core::store`) |
| retention | deletes bodies, passthrough rows and raw hardware samples past their windows, every 10 minutes; reads the policy from `Hot` each run |
| backend pollers | LM Studio and Ollama model lists and reachability, every 2 s (`gateway-backends`); the cloud providers are not polled because the gateway holds no keys |
| hardware sampler | sysinfo plus ioreg or nvidia-smi, every 2 s (`gateway-hardware`) |
| metrics loop | rolling windows from completed requests, republished every 2 s, one log line per feed row |
| alert dispatcher | turns findings, blocks and backend outages into alerts; stores, publishes and delivers them |

Shutdown gives open connections five seconds, under launchd's ten and systemd's fifteen.

### Hot reload

`crates/local-ai-gateway/src/hot.rs` holds what changes without a restart: the current `Config`, the DLP
`Engine`, the pricing table, the retention policy, the alert settings and the unified listener's routing
table (default backend, aliases, routes). `Hot::apply` validates a whole configuration (structure, pricing,
DLP compile, known-secrets file), swaps the engine into the proxy's scanner slot, the capture policy,
pricing and routing table into the proxy context, the alert settings and the retention policy, then
publishes `config_changed` with the sections that differ from the boot configuration and only take effect
on restart (`dashboard`, `backends.<name>`, `unified.listen`, `hardware`, `log`). CLI overrides (`--dashboard-listen`,
`--log-level`) are re-applied to every reload so a reload of the unchanged file is a no-op. `PUT /api/config`
writes the file atomically (write, rename) and applies; `POST /api/config/reload` and SIGHUP re-read it.
Errors leave everything as it was.

## Workspace

| Crate | Role |
|---|---|
| `gateway-core` | shared types, `config.toml` schema and validation, paths, event bus, aggregation, pricing, the routing table (`routing`), store |
| `gateway-proxy` | listeners (fixed and unified), classifier (`router`), resolver (`route`), request/response pipelines, parsers, upgrade tunnel, capture, the `RequestScanner` hook |
| `gateway-dlp` | rules, known-secret matcher, engine (JSON walk, raw scan, allowlist, fingerprints, previews), redaction, policy |
| `gateway-backends` | LM Studio and Ollama clients and pollers |
| `gateway-hardware` | sysinfo, ioreg, nvidia-smi, powermetrics |
| `local-ai-gateway` | the binary: CLI, wiring, hot reload, alert dispatcher, web layer, `service` and `setup` |

`frontend/` is a Vite + React + TypeScript + Tailwind app. `crates/local-ai-gateway/build.rs` stages
`frontend/dist` into `OUT_DIR/ui` (or a placeholder page when the frontend is not built) and rust-embed
compiles it in; it emits `rerun-if-changed` for every file under `dist/` so a frontend-only rebuild
re-embeds.

## Routing

A listener has a mode. `Fixed` sends everything to one backend, as the per-backend listeners always did.
`Unified` (`[unified]`, port 4000 by default) holds every enabled backend as a target and chooses per
request. The choice is made as early as the request allows:

1. From the path alone (`RequestMeta::new`): an explicit prefix (`/anthropic/v1/messages` → Anthropic,
   stripped before forwarding) or a provider-specific family (`/api/v0/*` → LM Studio, other `/api/*` →
   Ollama, `/v1beta/*` and `/v1/models/{model}:{method}` → Gemini). Anything else is provisionally the
   default backend and `decided = false`.
2. For an undecided request with a body (`inference`): the body is read first and the model decides, in
   this order: a configured alias (its target replaces `model` in the forwarded body), the live LM Studio
   and Ollama model lists (exact id, then Ollama's canonical spelling; this comes before the `provider/`
   reading because LM Studio ids are `publisher/model`), a `provider/model` spelling using one of the five
   backend ids (the prefix is stripped from `model`), the configured `[[unified.routes]]` prefixes (longest
   wins), the built-in patterns (`claude-*` Anthropic; `gemini-*` Gemini; `gpt-*`, `chatgpt-*`, `o1`/`o3`/`o4`,
   `text-embedding-3*` OpenAI; an Ollama `:cloud` tag Ollama), then a provider header, then the default.
3. For an undecided request without a body: a provider header (`anthropic-version` or `x-api-key` →
   Anthropic, `x-goog-api-key` → Gemini, `openai-organization`/`openai-project`/`openai-beta` → OpenAI);
   `GET /v1/models` with none of them is answered by the gateway with the merged list of the local servers'
   models and the aliases (`owned_by` names the backend); `GET /v1/models/{id}` resolves `{id}` like a model;
   anything else goes to the default.

Every record carries `routed_by` (`listener`, `prefix`, `path`, `alias`, `model_list`, `provider_prefix`,
`route`, `model_pattern`, `header`, `default`). The pure functions live in `gateway-proxy/src/route.rs`
(prefixes, families, hints, the live-list lookup, the `compatible` matrix, Gemini's compat path, the
credential guard, the merged list) and `gateway-core/src/routing.rs` (the configured table, which
`Config::validate` checks with the same code and `Hot` reloads live).

Once the target is a cloud provider: a format it does not serve is refused with 400 `format_not_served`
(Anthropic takes Messages and OpenAI chat; Gemini its own formats plus OpenAI chat and embeddings, which
are sent to `/v1beta/openai/…`; OpenAI the OpenAI family); a request carrying another provider's header
(`x-goog-api-key` to Anthropic, `anthropic-version` to Gemini, …) is refused with 400 `credential_mismatch`
so a key never reaches the wrong host; any request with a body outside the inference formats becomes
`cloud_passthrough` (buffered, scanned, forwarded, both bodies stored redacted, kept past the passthrough
retention, not a feed row); `accept-encoding` is removed so the answer can be parsed; a WebSocket upgrade is
refused with 400 because the tunnel cannot scan what it copies; and the request goes out through the second
HTTP client, which refuses to follow redirects (reqwest would carry `x-api-key` and `x-goog-api-key` to the
new host), so a 3xx is passed to the client and recorded as a failure. A completed request to a provider gets
`actual_cost_usd` from `pricing.toml` by the model id (`PricingTable::price_for_model`: lower-case, `.`→`-`,
longest key at a `-`/`:`/`@`/end boundary) and no hypothetical columns. Local targets accept every format
and header, as before. A backend the listener does not reach (disabled) is refused with 503
`backend_disabled`. Gateway errors to a provider's clients outside the inference formats take that
provider's own error shape.

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

- The proxy never changes the bytes a client receives. The request changes it makes are enumerated: the
  OpenAI `stream_options.include_usage` rewrite on streaming requests (every listener); and on the unified
  listener only, the explicit `/anthropic/`-style prefix stripped from the path, `model` rewritten when an
  alias or a `provider/model` spelling resolved it, `/v1/…` → `/v1beta/openai/…` for OpenAI-format requests
  to Gemini, and `accept-encoding` removed from requests to cloud providers.
- The gateway holds no provider keys. A client's credentials go only to the backend its request resolved
  to; a request whose headers belong to another provider is refused; a cloud provider's redirect is never
  followed.
- No request or response body, header value or access token is ever written to the log. Bodies reach
  only the SQLite capture store, DLP-redacted. `x-goog-api-key` joins the redacted headers; credential
  query parameters are redacted in the stored query and in logged URLs.
- A finding never carries the matched value: fingerprint and masked preview only. Nothing typed into the
  test box is stored or logged.
- Unknown `config.toml` keys are errors. Validation errors stop startup and refuse a reload; warnings are logged.
- `dashboard.auth = false` is only accepted on a loopback bind; `tls = "auto"` enables HTTPS off loopback.
- Tests need no terminal, sudo, GPU, LM Studio or Ollama; anything that touches the machine is `#[ignore]`.
- A reload either applies whole or changes nothing.

## Fixtures

`fixtures/` holds real captures, never hand-edited: LM Studio 0.4.25 (MLX engine, `qwen/qwen3.6-27b`) and
Ollama 0.40.6 (`gemma4:12b-mlx`) answering every inference format with a tiny `max_tokens`, plus model lists,
an `ioreg` dump, three Ollama error bodies and eight LM Studio context-overflow refusals (a 6000-token prompt
into a 2048-token context). What they settled: LM Studio sends a usage frame on
`/v1/chat/completions` and `/api/v0/chat/completions` streams only when `stream_options.include_usage` is set
(hence `rewrite_include_usage`, which now covers `LmStudioNative` too), puts `usage` on the last frame of
completion streams unasked, carries engine `stats` only on non-streaming `/api/v0` objects, and reports zero
prompt tokens for embeddings. Ollama splits an Anthropic prompt between `input_tokens` and
`cache_read_input_tokens` (the parser sums them), streams thinking as `thinking_delta` / `thinking` /
`response.reasoning_summary_text.delta`, and sends no usage on `/v1/completions` streams. Reasoning text from
every format is part of the reassembled reply, so DLP sees it. An oversized prompt is refused by LM Studio 0.4.25
with the "tokens to keep from the initial prompt" wording and no numbers: 400 on `/v1/chat/completions`,
`/v1/completions` and `/api/v0/chat/completions`, 500 on `/v1/messages` and `/v1/responses`, and an
`event: error` frame inside a 200 stream for all three streaming formats (the Responses stream also sends
`response.failed`). The pipeline treats any error status or error frame whose message parses as an overflow
as `rejected`, records the status the client received, and takes the context length from the latest model
list when the message has none.

`fixtures/anthropic` and `fixtures/gemini` were captured on 2026-10-24 from the real APIs with
`claude-haiku-5-5`, `gemini-3.5-flash-lite` and `gemini-embedding-2`, the same "Say hi." prompt and a small
`max_tokens`. What they settled: both providers' OpenAI-compatible chat endpoints send usage only when the
request carries `stream_options.include_usage` (Anthropic as a trailing frame with empty `choices`, Gemini on
every chunk with cumulative counts), so `rewrite_include_usage` is on for every backend; Anthropic's
`message_delta` repeats `input_tokens` beside the final `output_tokens` and the stream carries a `ping`
event; Gemini's last SSE frame holds an empty text part with only a `thoughtSignature`, its `usageMetadata`
repeats on every frame, reasoning is billed as `thoughtsTokenCount` with no thought part returned even when
`includeThoughts` is set (so `gen_tokens` is candidates plus thoughts and a thought part in the text is
tested on the documented shape), and `:embedContent` reports `usageMetadata.promptTokenCount`. Both
unknown-model refusals are 404s carrying the providers' ordinary error objects; Google answers an `alt=sse`
request's error with a bare JSON object under `text/event-stream`, which the error branch parses like any
other body. The Gemini parser (`parsers/gemini.rs`) reads SSE frames with `alt=sse`, one object, or a JSON
array parsed when complete. A second set the same day covered the shapes that depend on the model: Opus 5.5
reasoning on a puzzle (the thinking block is one empty `thinking_delta` plus a `signature_delta` by
default, the summary text with `display: summarized`; `output_tokens` already counts the thinking), a Haiku
pair with a 15k-token cached system prompt (`cache_creation_input_tokens`, then `cache_read_input_tokens`,
both part of the prompt count), `gemini-4-argon` at `thinkingLevel: high` returning a part marked `thought`
before the answer (Google's prompt count rises on the final frame; the last frame wins), and
`gemini-3-pro-image` declining a prompt on recitation grounds with a 200 whose candidate has an empty
`content` and no candidate count, since Google omits zero counts: the parser then takes `totalTokenCount`
minus `promptTokenCount`. Fable 5.1 chose not to think on either prompt it was given, so its streams were
plain and were not kept. There is no OpenAI capture: the OpenAI shapes are covered by the other four
servers.

## Known gaps

- A rejected record from LM Studio 0.4.25 has no `input_tokens`, and its `context_length` is a lookup in the
  latest model list, so it is `None` when the model was unloaded before the next poll. The llama.cpp wording
  with numbers, from the terminal monitors' logs, is kept for `llmster`.
- An error inside a 2xx stream is recorded as `failed` or `rejected` with the 200 status the client already
  received.
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
- No format translation: a request reaches a cloud provider in a format it serves or is refused. A
  model-less request with a body and no provider header goes to `default_backend`.
- The cloud providers are not polled, so they show no reachability, version or model list; the merged
  `GET /v1/models` answer is gateway-generated and not recorded (`RequestRecord.backend` is not optional).
- A Gemini stream without `alt=sse` is parsed once the array is complete, so its figures arrive at the end.
- WebSocket upgrades to cloud providers are refused on every listener (Gemini Live, OpenAI Realtime).
- Real spend is flat list price: cache discounts and tiers are not applied, Ollama `:cloud` has no price.
- Schema v2 rows name the cloud providers; an older build fails whole page queries on them (`rows.rs`
  parses `backend` strictly), so the database file must not be shared with or downgraded to one.
- The reqwest root store is webpki; a corporate TLS interceptor in front of the providers would need
  `rustls-tls-native-roots`. Cloud uploads above the 64 MiB request cap get 413.
