import { useState } from 'react'
import { useQueryClient } from '@tanstack/react-query'
import { useBody, useRequestDetail } from '../../api/hooks'
import { BACKEND_NAMES, type RequestDetail as Detail } from '../../api/types'
import { copyAsCurl } from '../../lib/curl'
import { formatBytes, formatInt, formatMs, formatSeconds, formatTps, formatUsd } from '../../lib/format'
import BodyView from './BodyView'
import ChunkTimeline from './ChunkTimeline'
import FindingsTable from './FindingsTable'

type Tab = 'overview' | 'request' | 'response' | 'headers' | 'timeline' | 'findings'
const TABS: Tab[] = ['overview', 'request', 'response', 'headers', 'timeline', 'findings']

/** `rejected (input 6000 tokens, context 2048)`, with only the parts the backend reported. */
function rejectedText(input: number | null, context: number | null): string {
  const parts: string[] = []
  if (input !== null) parts.push(`input ${input} tokens`)
  if (context !== null) parts.push(`context ${context}`)
  return parts.length ? `rejected (${parts.join(', ')})` : 'rejected (prompt longer than the loaded context)'
}

function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <>
      <dt className="text-zinc-500">{label}</dt>
      <dd className="break-all">{children}</dd>
    </>
  )
}

function Overview({ detail }: { detail: Detail }) {
  const r = detail.record
  const outcome = r.outcome
  const outcomeText =
    outcome.kind === 'completed'
      ? 'completed'
      : outcome.kind === 'failed'
        ? `failed (HTTP ${outcome.status}, ${outcome.source})`
        : outcome.kind === 'rejected'
          ? rejectedText(outcome.input_tokens, outcome.context_length)
          : outcome.kind === 'blocked'
            ? `blocked by DLP rule ${outcome.rule_id}`
            : 'client aborted'
  const costs = Object.entries(r.hypothetical_cost)
  return (
    <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 font-mono text-xs">
      <Row label="id">{r.id}</Row>
      <Row label="backend">
        {BACKEND_NAMES[r.backend]} via {r.listener}
        {r.routed_by && r.routed_by !== 'listener' ? ` · routed by ${r.routed_by.replace('_', ' ')}` : ''}
      </Row>
      <Row label="request">
        {r.method} {r.path}
        {r.query ? `?${r.query}` : ''} · {r.api_format}
        {r.streamed ? ' · streamed' : ''}
        {r.kind !== 'http' ? ` · ${r.kind}` : ''}
      </Row>
      <Row label="model">
        {r.model_resolved ?? r.model_requested ?? '-'}
        {r.model_requested && r.model_resolved && r.model_requested !== r.model_resolved ? ` (requested ${r.model_requested})` : ''}
        {r.is_cloud ? ' · cloud' : ''}
      </Row>
      <Row label="client">
        {r.client_ip}
        {r.user_agent ? ` · ${r.user_agent}` : ''}
        {r.client_request_id ? ` · x-request-id ${r.client_request_id}` : ''}
      </Row>
      <Row label="started">{new Date(r.started_at).toLocaleString()}</Row>
      <Row label="ended">
        {new Date(r.ended_at).toLocaleString()} ({formatSeconds(r.total_ms)})
      </Row>
      <Row label="status">
        {r.status ?? '-'} · {outcomeText}
      </Row>
      {r.error_summary && <Row label="error">{r.error_summary}</Row>}
      <Row label="tokens">
        {r.tokens_estimated ? '~' : ''}
        {formatInt(r.prompt_tokens)} prompt / {r.tokens_estimated ? '~' : ''}
        {formatInt(r.gen_tokens)} generated
        {r.stop_reason ? ` · stop ${r.stop_reason}` : ''}
      </Row>
      <Row label="timings">
        TTFT {formatMs(r.ttft_ms)} · {formatTps(r.tokens_per_sec)} tok/s{r.timings_estimated ? ' (gateway clock)' : ' (engine)'}
      </Row>
      <Row label="bytes">
        {formatBytes(r.request_bytes)} in / {formatBytes(r.response_bytes)} out
      </Row>
      {r.actual_cost_usd !== null && <Row label="cost">{formatUsd(r.actual_cost_usd)} at the provider's list price</Row>}
      {costs.length > 0 && (
        <Row label="hypothetical cost">
          {costs.map(([k, v]) => `${k} ${formatUsd(v)}`).join(' · ')}
        </Row>
      )}
      <Row label="bodies">
        {detail.request_body ? `request ${formatBytes(detail.request_body.stored_bytes)}${detail.request_body.redacted ? ' (redacted)' : ''}` : 'no request body'} ·{' '}
        {detail.response_body ? `response ${formatBytes(detail.response_body.stored_bytes)}${detail.response_body.truncated ? ' (truncated)' : ''}` : 'no response body'}
      </Row>
    </dl>
  )
}

function Headers({ detail }: { detail: Detail }) {
  const table = (title: string, rows: [string, string][]) => (
    <div>
      <h3 className="mb-1 text-xs uppercase tracking-wider text-zinc-500">{title}</h3>
      {rows.length === 0 ? (
        <p className="text-xs text-zinc-500">none recorded</p>
      ) : (
        <table className="w-full font-mono text-xs">
          <tbody>
            {rows.map(([k, v], i) => (
              <tr key={i} className="border-t border-zinc-800/60">
                <td className="py-0.5 pr-3 text-zinc-400">{k}</td>
                <td className="py-0.5 break-all">{v}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  )
  return (
    <div className="grid gap-4 md:grid-cols-2">
      {table('request headers', detail.record.request_headers)}
      {table('response headers', detail.record.response_headers)}
    </div>
  )
}

export default function RequestDetail({ id }: { id: string }) {
  const detail = useRequestDetail(id)
  const [tab, setTab] = useState<Tab>('overview')
  const [copied, setCopied] = useState(false)
  const queryClient = useQueryClient()
  const record = detail.data?.record
  const requestBody = useBody(id, 'request', !!detail.data?.request_body)
  const responseBody = useBody(id, 'response', !!detail.data?.response_body)

  if (detail.isPending) return <p className="text-sm text-zinc-500">Loading…</p>
  if (detail.isError || !detail.data || !record) return <p className="text-sm text-red-300">{(detail.error as Error | null)?.message ?? 'not found'}</p>

  const copyCurl = async () => {
    const command = copyAsCurl(record, requestBody.data?.text ?? null)
    try {
      await navigator.clipboard.writeText(command)
      setCopied(true)
      setTimeout(() => setCopied(false), 1500)
    } catch {
      window.prompt('copy the command', command)
    }
  }
  const deleteBodies = async () => {
    if (!window.confirm('Delete the stored request and response bodies for this request? The metadata stays.')) return
    await fetch(`/api/requests/${id}/bodies`, { method: 'DELETE', credentials: 'same-origin' })
    await queryClient.invalidateQueries({ queryKey: ['request', id] })
    await queryClient.invalidateQueries({ queryKey: ['body', id] })
    await queryClient.invalidateQueries({ queryKey: ['requests'] })
  }

  return (
    <div>
      <div className="mb-2 flex flex-wrap items-center gap-1 text-xs">
        {TABS.map((t) => (
          <button key={t} type="button" onClick={() => setTab(t)} aria-pressed={tab === t} className={`rounded px-2 py-1 ${tab === t ? 'bg-zinc-700 text-zinc-50' : 'text-zinc-400 hover:bg-zinc-800'}`}>
            {t}
            {t === 'findings' && detail.data.findings.length > 0 ? ` (${detail.data.findings.length})` : ''}
          </button>
        ))}
        <span className="ml-auto flex gap-1">
          <button type="button" onClick={copyCurl} className="rounded px-2 py-1 text-zinc-300 hover:bg-zinc-800" title="curl against the gateway listener; credentials redacted">
            {copied ? 'copied' : 'copy as curl'}
          </button>
          {detail.data.request_body && (
            <a className="rounded px-2 py-1 text-zinc-300 hover:bg-zinc-800" href={`/api/requests/${id}/body/request?download=true`}>
              download request
            </a>
          )}
          {detail.data.response_body && (
            <a className="rounded px-2 py-1 text-zinc-300 hover:bg-zinc-800" href={`/api/requests/${id}/body/response?download=true`}>
              download response
            </a>
          )}
          {(detail.data.request_body || detail.data.response_body) && (
            <button type="button" onClick={deleteBodies} className="rounded px-2 py-1 text-red-300 hover:bg-zinc-800">
              delete bodies
            </button>
          )}
        </span>
      </div>
      {tab === 'overview' && <Overview detail={detail.data} />}
      {tab === 'request' && <BodyView body={requestBody.data} loading={requestBody.isPending && !!detail.data.request_body} format={record.api_format} direction="request" streamed={false} />}
      {tab === 'response' && <BodyView body={responseBody.data} loading={responseBody.isPending && !!detail.data.response_body} format={record.api_format} direction="response" streamed={record.streamed} />}
      {tab === 'headers' && <Headers detail={detail.data} />}
      {tab === 'timeline' && <ChunkTimeline record={record} />}
      {tab === 'findings' && <FindingsTable findings={detail.data.findings} />}
    </div>
  )
}
