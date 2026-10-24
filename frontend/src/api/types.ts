// Mirrors of the Rust serde shapes in gateway-core and the dashboard API. Kept in step by hand.

export type Backend = 'lmstudio' | 'ollama' | 'anthropic' | 'gemini' | 'openai'
export type Platform = 'macos' | 'linux' | 'other'
export type ApiFormat =
  | 'ollama_native'
  | 'ollama_embeddings'
  | 'openai_chat'
  | 'openai_completions'
  | 'openai_embeddings'
  | 'openai_responses'
  | 'anthropic_messages'
  | 'lmstudio_native'
  | 'lmstudio_embeddings'
  | 'gemini_generate'
  | 'gemini_embed'
  | 'passthrough'
  | 'cloud_passthrough'
export type RequestKind = 'http' | 'upgrade' | 'load'
export type Severity = 'low' | 'medium' | 'high' | 'critical'
export type ModelState = 'loaded' | 'not-loaded' | 'cloud'

export type Outcome =
  | { kind: 'completed' }
  | { kind: 'failed'; status: number; source: 'upstream' | 'gateway' }
  | { kind: 'rejected'; input_tokens: number | null; context_length: number | null }
  | { kind: 'blocked'; rule_id: string }
  | { kind: 'client_aborted' }

export interface RequestRecord {
  id: string
  session_id: number
  backend: Backend
  listener: string
  kind: RequestKind
  client_ip: string
  user_agent: string | null
  client_request_id: string | null
  method: string
  path: string
  query: string | null
  api_format: ApiFormat
  streamed: boolean
  model_requested: string | null
  model_resolved: string | null
  is_cloud: boolean
  started_at: string
  first_byte_at: string | null
  ended_at: string
  status: number | null
  outcome: Outcome
  error_summary: string | null
  prompt_tokens: number | null
  gen_tokens: number | null
  tokens_estimated: boolean
  ttft_ms: number | null
  total_ms: number
  tokens_per_sec: number | null
  timings_estimated: boolean
  stop_reason: string | null
  request_bytes: number
  response_bytes: number
  request_headers: [string, string][]
  response_headers: [string, string][]
  chunk_timeline: [number, number][]
  finding_count: number
  max_severity: Severity | null
  hypothetical_cost: Record<string, number>
  /** Real cost at the provider's list price; only completed requests to a cloud provider have one. */
  actual_cost_usd: number | null
  /** How the backend was chosen: `listener` on a per-backend listener, the unified listener's reason otherwise. */
  routed_by: string | null
}

export interface RequestSummary extends RequestRecord {
  has_request_body: boolean
  has_response_body: boolean
}

export interface RequestPage {
  items: RequestSummary[]
  next_cursor: string | null
}

export interface InflightRequest {
  id: string
  backend: Backend
  api_format: ApiFormat
  kind: RequestKind
  method: string
  path: string
  model_requested: string | null
  is_cloud: boolean
  client_ip: string
  user_agent: string | null
  started_at: string
}

export interface ModelInfo {
  backend: Backend
  id: string
  kind: string | null
  compatibility_type: string | null
  quantization: string | null
  max_context_length: number | null
  loaded_context_length: number | null
  state: ModelState
  publisher: string | null
  arch: string | null
  capabilities: string[]
  size_bytes: number | null
  size_vram_bytes: number | null
  expires_at: string | null
  remote_host: string | null
}

export interface BackendSnapshot {
  backend: Backend
  reachable: boolean
  version: string | null
  error: string | null
  models: ModelInfo[]
  polled_at: string
}

export interface ProcStats {
  process_count: number
  cpu_pct: number
  rss_bytes: number
}

export interface HardwareSample {
  at: string
  system_cpu_pct: number
  mem_used_bytes: number
  mem_total_bytes: number
  mem_available_bytes: number
  lmstudio: ProcStats
  ollama: ProcStats
  gpu_util_pct: number | null
  gpu_renderer_pct: number | null
  gpu_tiler_pct: number | null
  gpu_mem_used_bytes: number | null
  gpu_mem_alloc_bytes: number | null
  ane_power_mw: number | null
}

export interface WindowStats {
  request_count: number
  prompt_tokens: number
  gen_tokens: number
  mean_tps: number
  p50_tps: number
  p95_tps: number
  mean_ttft_ms: number
}

export interface RollingMetrics {
  one_minute: WindowStats
  five_minute: WindowStats
  fifteen_minute: WindowStats
  session: WindowStats
  per_model: Record<string, WindowStats>
  per_backend: Record<string, WindowStats>
  session_started_at: string
  computed_at: string
}

export interface ListenerStatus {
  listen: string
  /** The unified listener picks the backend per request. */
  unified: boolean
  /** The one backend of a fixed listener. */
  backend: Backend | null
  upstream: string | null
  /** Every backend the listener can send a request to. */
  targets: Backend[]
  bound: boolean
}

export interface BackendStatus {
  backend: Backend
  name: string
  enabled: boolean
  listen: string[]
  upstream: string
  reachable: boolean | null
  version: string | null
  error: string | null
  polled_at: string | null
  models_total: number
  models_loaded: number
  models_cloud: number
}

export interface DlpStatus {
  enabled: boolean
  rules_total: number
  rules_enabled: number
  custom_rules: number
  known_secrets: number
  allowlist_entries: number
  findings_24h: number
  blocked_24h: number
  cloud_requests_24h: number
}

export interface PathsStatus {
  data_dir: string
  config_file: string
  config_exists: boolean
  token_file: string
  db_file: string
  known_secrets_file: string
  logs_dir: string
}

export interface Status {
  version: string
  platform: Platform
  started_at: string
  uptime_s: number
  session_id: number | null
  dashboard: { listen: string; tls: boolean; auth: boolean }
  listeners: ListenerStatus[]
  backends: BackendStatus[]
  db: { path: string; size_bytes: number; bodies: number; retention_days: number } | null
  inflight: number
  unacked_alerts: number
  dlp: DlpStatus
  paths: PathsStatus
}

export interface SeriesPoint {
  t: string
  values: Record<string, number>
}

export type SeriesMetric =
  | 'req_per_min'
  | 'tok_per_sec'
  | 'ttft'
  | 'tokens_by_model'
  | 'tokens_by_client'
  | 'cost_cumulative'

export interface Series {
  metric: SeriesMetric
  from: string
  to: string
  bucket_ms: number
  points: SeriesPoint[]
}

export interface HypotheticalCost {
  input_usd: number
  output_usd: number
  total_usd: number
}

export interface CostTable {
  from: string
  to: string
  prompt_tokens: number
  gen_tokens: number
  costs: Record<string, HypotheticalCost>
  columns: string[]
}

export interface Lifetime {
  session_count: number
  request_count: number
  prompt_tokens: number
  gen_tokens: number
  session_started_at: string
}

export interface HardwarePoint {
  t: string
  cpu: number | null
  mem_used: number | null
  mem_available: number | null
  mem_total: number | null
  gpu: number | null
  gpu_mem: number | null
  ane: number | null
  lms_cpu: number | null
  lms_rss: number | null
  ollama_cpu: number | null
  ollama_rss: number | null
}

export interface HelloMessage {
  type: 'hello'
  session_id: number | null
  server_time: string
  version: string
  platform: Platform
  started_at: string
  inflight: InflightRequest[]
}

export type ServerMessage =
  | HelloMessage
  | { type: 'pong' }
  | { type: 'subscribed'; topics: string[] }
  | { type: 'resync'; dropped: number }
  | { type: 'shutdown' }
  | { type: 'config_changed'; restart_required: string[] }
  | { type: 'request_started'; request: InflightRequest }
  | { type: 'request_first_byte'; id: string; ttft_ms: number }
  | { type: 'request_progress'; id: string; gen_tokens_estimate: number; response_bytes: number }
  | { type: 'request_completed'; record: RequestRecord }
  | { type: 'finding'; finding: Finding }
  | { type: 'alert'; alert: Alert }
  | { type: 'backend_snapshot'; snapshot: BackendSnapshot }
  | { type: 'hardware'; sample: HardwareSample }
  | { type: 'metrics'; rolling: RollingMetrics }

export const BACKEND_NAMES: Record<Backend, string> = {
  lmstudio: 'LM Studio',
  ollama: 'Ollama',
  anthropic: 'Anthropic',
  gemini: 'Gemini',
  openai: 'OpenAI',
}
/** The servers on this machine: polled, with their own listeners and process stats. */
export const LOCAL_BACKENDS: Backend[] = ['lmstudio', 'ollama']
/** The hosted APIs: every request to one leaves the machine. */
export const CLOUD_BACKENDS: Backend[] = ['anthropic', 'gemini', 'openai']

export type FindingCategory = 'secret' | 'pii' | 'known_secret' | 'custom'
export type DlpAction = 'record' | 'alert' | 'block'

export interface Finding {
  request_id: string
  direction: 'request' | 'response'
  rule_id: string
  category: FindingCategory
  severity: Severity
  location: string
  offset: number | null
  length: number
  fingerprint: string
  preview: string
  action: DlpAction
  created_at: string
}

export interface BodyMeta {
  content_type: string | null
  original_bytes: number
  stored_bytes: number
  truncated: boolean
  redacted: boolean
  is_text: boolean
}

export interface RequestDetail {
  record: RequestRecord
  request_body: BodyMeta | null
  response_body: BodyMeta | null
  findings: Finding[]
}

export const API_FORMATS: ApiFormat[] = [
  'ollama_native',
  'ollama_embeddings',
  'openai_chat',
  'openai_completions',
  'openai_embeddings',
  'openai_responses',
  'anthropic_messages',
  'lmstudio_native',
  'lmstudio_embeddings',
  'gemini_generate',
  'gemini_embed',
  'passthrough',
  'cloud_passthrough',
]

export const OUTCOMES = ['completed', 'failed', 'rejected', 'blocked', 'client_aborted'] as const

export type ResponseAction = 'record' | 'alert'
export type Direction = 'request' | 'response'

/** A finding joined with its request, as `/api/findings` lists them. */
export interface FindingRow extends Finding {
  id: number
  backend: Backend
  model: string | null
  is_cloud: boolean
}

export interface FindingsPage {
  items: FindingRow[]
  next_before: number | null
}

export type AlertKind = 'dlp_finding' | 'dlp_blocked' | 'cloud_egress' | 'backend_unreachable' | 'backend_recovered'

export interface Alert {
  id: number
  created_at: string
  kind: AlertKind
  severity: Severity
  title: string
  detail: Record<string, unknown>
  request_id: string | null
  acknowledged_at: string | null
  delivered: string[]
}

export interface AlertsPage {
  items: Alert[]
  next_before: number | null
  unacked: number
}

export interface RuleView {
  id: string
  description: string
  category: FindingCategory
  severity: Severity
  builtin: boolean
  enabled: boolean
  redact: boolean
  keywords: string[]
  min_entropy: number | null
  validator: string | null
  /** Per backend id (cloud providers resolved as cloud egress) plus `cloud` for Ollama cloud models. */
  request_action: Record<string, DlpAction>
  response_action: Record<string, ResponseAction>
}

export interface RulesView {
  enabled: boolean
  default_action: DlpAction
  response_action: ResponseAction
  cloud: { request_action: DlpAction; pii_action: DlpAction }
  notify_min_severity: Severity
  redact_stored_copies: boolean
  known_secrets: number
  known_secrets_file: string
  allowlist_entries: number
  custom_rules: number
  rules: RuleView[]
}

export interface DlpTestResult {
  enabled: boolean
  findings: Finding[]
  would_block: string | null
  redacted: string | null
  allowlisted: number
  scan_truncated: boolean
}

export interface ApplyReport {
  applied: string[]
  restart_required: string[]
  warnings: string[]
  known_secrets: number
  rules_enabled: number
}

export interface ConfigFile {
  path: string
  exists: boolean
  text: string
  sample: string
}

export interface IssueView {
  level: 'error' | 'warning'
  message: string
}

export interface ConfigCheck {
  ok: boolean
  issues: IssueView[]
  saved: boolean
  report: ApplyReport | null
}

export interface RetentionReport {
  bodies_deleted: number
  passthrough_records_deleted: number
  hardware_samples_deleted: number
  alerts_deleted: number
}

export const SEVERITIES: Severity[] = ['low', 'medium', 'high', 'critical']
