import { keepPreviousData, useInfiniteQuery, useQuery } from '@tanstack/react-query'
import { apiGet, apiSend } from './client'
import type {
  AlertsPage,
  ApplyReport,
  Backend,
  BackendGroup,
  ConfigCheck,
  ConfigFile,
  CostTable,
  DlpTestResult,
  FindingsPage,
  RetentionReport,
  RulesView,
  HardwarePoint,
  Lifetime,
  ModelInfo,
  RequestDetail,
  RequestPage,
  RollingMetrics,
  Series,
  SeriesMetric,
  Spend,
  Status,
} from './types'

function qs(params: Record<string, string | number | boolean | undefined | null>): string {
  const parts = Object.entries(params)
    .filter(([, v]) => v !== undefined && v !== null && v !== '')
    .map(([k, v]) => `${encodeURIComponent(k)}=${encodeURIComponent(String(v))}`)
  return parts.length ? `?${parts.join('&')}` : ''
}

/** What a metrics or request query is scoped to: one backend, one family of backends, or everything. */
export interface Scope {
  backend?: Backend
  group?: BackendGroup
}

const scopeParams = (scope?: Scope) => ({ backend: scope?.backend, group: scope?.group })
const scopeKey = (scope?: Scope) => `${scope?.backend ?? 'all'}|${scope?.group ?? 'all'}`

export function useStatus() {
  return useQuery({
    queryKey: ['status'],
    queryFn: () => apiGet<Status>('/api/status'),
    refetchInterval: 5000,
    placeholderData: keepPreviousData,
  })
}

export function useModels(backend?: Backend) {
  return useQuery({
    queryKey: ['models', backend ?? 'all'],
    queryFn: () => apiGet<ModelInfo[]>(`/api/models${qs({ backend })}`),
    refetchInterval: 5000,
    placeholderData: keepPreviousData,
  })
}

/** Rolling windows for a scope come from REST; the all-backends snapshot also arrives over the socket. */
export function useRolling(scope?: Scope) {
  return useQuery({
    queryKey: ['rolling', scopeKey(scope)],
    queryFn: () => apiGet<RollingMetrics>(`/api/metrics/rolling${qs(scopeParams(scope))}`),
    refetchInterval: 2000,
    placeholderData: keepPreviousData,
  })
}

export function useLifetime() {
  return useQuery({
    queryKey: ['lifetime'],
    queryFn: () => apiGet<Lifetime>('/api/metrics/lifetime'),
    refetchInterval: 5000,
    placeholderData: keepPreviousData,
  })
}

export function useCost(from: string | undefined, scope?: Scope) {
  return useQuery({
    queryKey: ['cost', from ?? '', scopeKey(scope)],
    queryFn: () => apiGet<CostTable>(`/api/metrics/cost${qs({ from, ...scopeParams(scope) })}`),
    enabled: !!from,
    refetchInterval: 5000,
    placeholderData: keepPreviousData,
  })
}

/** What the cloud providers charged at list price since `from`. */
export function useSpend(from: string | undefined) {
  return useQuery({
    queryKey: ['spend', from ?? ''],
    queryFn: () => apiGet<Spend>(`/api/metrics/spend${qs({ from })}`),
    enabled: !!from,
    refetchInterval: 5000,
    placeholderData: keepPreviousData,
  })
}

export function useSeries(metric: SeriesMetric, fromIso: string, scope?: Scope, model?: string) {
  return useQuery({
    queryKey: ['series', metric, fromIso, scopeKey(scope), model ?? ''],
    queryFn: () => apiGet<Series>(`/api/metrics/series${qs({ metric, from: fromIso, ...scopeParams(scope), model })}`),
    refetchInterval: 10000,
    placeholderData: keepPreviousData,
  })
}

export function useHardwareSeries(fromIso: string) {
  return useQuery({
    queryKey: ['hardware-series', fromIso],
    queryFn: () => apiGet<HardwarePoint[]>(`/api/hardware/series${qs({ from: fromIso })}`),
    refetchInterval: 10000,
    placeholderData: keepPreviousData,
  })
}

/** The most recent completed rows, to fill the feed before live events arrive. */
export function useRecentRequests(backend?: Backend) {
  return useQuery({
    queryKey: ['recent-requests', backend ?? 'all'],
    queryFn: () => apiGet<RequestPage>(`/api/requests${qs({ backend, limit: 30 })}`),
    staleTime: Infinity,
  })
}

export interface RequestListParams {
  backend?: Backend
  model?: string
  outcome?: string
  format?: string
  client?: string
  cloud?: boolean
  has_findings?: boolean
  from?: string
  q?: string
  include_passthrough?: boolean
}

export function requestListUrl(params: RequestListParams, cursor?: string): string {
  return `/api/requests${qs({ ...params, cursor, limit: 50 })}`
}

export function useRequestList(params: RequestListParams) {
  return useInfiniteQuery({
    queryKey: ['requests', params],
    queryFn: ({ pageParam }) => apiGet<RequestPage>(requestListUrl(params, pageParam || undefined)),
    initialPageParam: '',
    getNextPageParam: (last) => last.next_cursor ?? undefined,
    refetchInterval: 5000,
    placeholderData: keepPreviousData,
  })
}

export function useRequestDetail(id: string | undefined) {
  return useQuery({
    queryKey: ['request', id],
    queryFn: () => apiGet<RequestDetail>(`/api/requests/${id}`),
    enabled: !!id,
  })
}

export interface BodyText {
  text: string
  truncated: boolean
  redacted: boolean
  original_bytes: number
  content_type: string
}

export async function fetchBody(id: string, direction: 'request' | 'response'): Promise<BodyText | null> {
  const res = await fetch(`/api/requests/${id}/body/${direction}`, { credentials: 'same-origin' })
  if (res.status === 404) return null
  if (!res.ok) throw new Error(`HTTP ${res.status}`)
  return {
    text: await res.text(),
    truncated: res.headers.get('x-body-truncated') === 'true',
    redacted: res.headers.get('x-body-redacted') === 'true',
    original_bytes: Number(res.headers.get('x-body-original-bytes') ?? 0),
    content_type: res.headers.get('content-type') ?? 'text/plain',
  }
}

export function useBody(id: string | undefined, direction: 'request' | 'response', available: boolean) {
  return useQuery({
    queryKey: ['body', id, direction],
    queryFn: () => fetchBody(id as string, direction),
    enabled: !!id && available,
    staleTime: Infinity,
  })
}

export interface FindingListParams {
  rule?: string
  severity?: string
  backend?: Backend
  direction?: string
  from?: string
}

export function useFindings(params: FindingListParams) {
  return useInfiniteQuery({
    queryKey: ['findings', params],
    queryFn: ({ pageParam }) => apiGet<FindingsPage>(`/api/findings${qs({ ...params, before: pageParam || undefined, limit: 50 })}`),
    initialPageParam: 0,
    getNextPageParam: (last) => last.next_before ?? undefined,
    refetchInterval: 10000,
    placeholderData: keepPreviousData,
  })
}

export function useAlerts(unackedOnly: boolean) {
  return useInfiniteQuery({
    queryKey: ['alerts', unackedOnly],
    queryFn: ({ pageParam }) => apiGet<AlertsPage>(`/api/alerts${qs({ unacked: unackedOnly ? true : undefined, before: pageParam || undefined, limit: 50 })}`),
    initialPageParam: 0,
    getNextPageParam: (last) => last.next_before ?? undefined,
    refetchInterval: 10000,
    placeholderData: keepPreviousData,
  })
}

export function useDlpRules() {
  return useQuery({
    queryKey: ['dlp-rules'],
    queryFn: () => apiGet<RulesView>('/api/dlp/rules'),
    staleTime: 60_000,
  })
}

export function useConfigFile() {
  return useQuery({
    queryKey: ['config-file'],
    queryFn: () => apiGet<ConfigFile>('/api/config/file'),
    staleTime: Infinity,
  })
}

export const actions = {
  ackAlert: (id: number) => apiSend<{ acknowledged: number }>('POST', `/api/alerts/${id}/ack`),
  ackAllAlerts: () => apiSend<{ acknowledged: number }>('POST', '/api/alerts/ack-all'),
  dlpTest: (text: string, backend: Backend, isCloud: boolean) =>
    apiSend<DlpTestResult>('POST', '/api/dlp/test', { text, backend, is_cloud: isCloud }),
  reloadConfig: () => apiSend<ApplyReport>('POST', '/api/config/reload'),
  validateConfig: (text: string) => apiSend<ConfigCheck>('POST', '/api/config/validate', { text }),
  saveConfig: (text: string) => apiSend<ConfigCheck>('PUT', '/api/config', { text }),
  runRetention: () => apiSend<RetentionReport>('POST', '/api/retention/run'),
  resetSession: () => apiSend<{ session_started_at: string }>('POST', '/api/session/reset'),
}
