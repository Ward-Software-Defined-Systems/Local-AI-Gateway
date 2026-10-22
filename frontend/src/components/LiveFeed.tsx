import { Link } from 'react-router'
import type { Backend, InflightRequest, ModelInfo, RequestRecord } from '../api/types'
import { BACKEND_NAMES } from '../api/types'
import { formatMs, formatTime, formatTps, truncate } from '../lib/format'
import { useLive } from '../store/live'

interface Props {
  rows: RequestRecord[]
  inflight: InflightRequest[]
  models: ModelInfo[]
  showBackend?: boolean
  backend?: Backend
}

/** LM Studio's rule: the reply filled the context when the stop says so, or hit the token cap at the loaded length. */
function filledContext(record: RequestRecord, models: ModelInfo[]): boolean {
  if (record.stop_reason === 'contextLengthReached') return true
  if (record.stop_reason !== 'maxPredictedTokensReached') return false
  const model = models.find((m) => m.backend === record.backend && m.id === record.model_resolved)
  const ctx = model?.loaded_context_length
  return !!ctx && (record.prompt_tokens ?? 0) + (record.gen_tokens ?? 0) >= ctx
}

function outcomeText(record: RequestRecord): string {
  const o = record.outcome
  switch (o.kind) {
    case 'completed':
      return record.stop_reason ?? '-'
    case 'failed':
      return `${o.source === 'gateway' ? 'gateway' : 'failed'} (HTTP ${o.status})`
    case 'rejected':
      return o.context_length ? `rejected (ctx ${o.context_length})` : 'rejected'
    case 'blocked':
      return `blocked (${o.rule_id})`
    case 'client_aborted':
      return 'aborted'
  }
}

export default function LiveFeed({ rows, inflight, models, showBackend = false, backend }: Props) {
  const paused = useLive((s) => s.paused)
  const skipped = useLive((s) => s.skippedWhilePaused)
  const togglePause = useLive((s) => s.togglePause)
  const resetFeed = useLive((s) => s.resetFeed)
  const visibleInflight = inflight
    .filter((r) => r.api_format !== 'passthrough' && r.kind === 'http' && (backend === undefined || r.backend === backend))
    .sort((a, b) => (a.started_at < b.started_at ? 1 : -1))
  return (
    <div>
      <div className="mb-2 flex items-center gap-2 text-xs">
        <span className="text-zinc-500">
          {rows.length} row{rows.length === 1 ? '' : 's'}
          {paused && skipped > 0 ? ` · ${skipped} skipped while paused` : ''}
        </span>
        <button type="button" onClick={togglePause} className="ml-auto rounded px-2 py-0.5 text-zinc-300 hover:bg-zinc-800">
          {paused ? 'resume' : 'pause'}
        </button>
        <button
          type="button"
          onClick={() => {
            resetFeed()
            void fetch('/api/session/reset', { method: 'POST', credentials: 'same-origin' })
          }}
          className="rounded px-2 py-0.5 text-zinc-300 hover:bg-zinc-800"
          title="Clear the feed and the rolling windows; the database keeps everything"
        >
          reset session
        </button>
      </div>
      <div className="max-h-96 overflow-auto">
        <table className="w-full font-mono text-sm">
          <thead className="sticky top-0 bg-zinc-900 text-left text-xs text-zinc-500">
            <tr>
              <th className="pb-1 pr-3 font-normal">time</th>
              {showBackend && <th className="pb-1 pr-3 font-normal">backend</th>}
              <th className="pb-1 pr-3 font-normal">model</th>
              <th className="pb-1 pr-3 text-right font-normal">prompt</th>
              <th className="pb-1 pr-3 text-right font-normal">gen</th>
              <th className="pb-1 pr-3 text-right font-normal">TTFT</th>
              <th className="pb-1 pr-3 text-right font-normal">tok/s</th>
              <th className="pb-1 font-normal">stop</th>
            </tr>
          </thead>
          <tbody>
            {visibleInflight.map((r) => (
              <tr key={r.id} className="border-t border-zinc-800/60 text-zinc-400 italic">
                <td className="py-1 pr-3">{formatTime(r.started_at)}</td>
                {showBackend && <td className="py-1 pr-3">{BACKEND_NAMES[r.backend]}</td>}
                <td className="py-1 pr-3" title={r.model_requested ?? undefined}>
                  {truncate(r.model_requested ?? '?')}
                  {r.is_cloud && <span className="ink-cloud"> ☁</span>}
                </td>
                <td className="py-1 pr-3 text-right">…</td>
                <td className="py-1 pr-3 text-right">…</td>
                <td className="py-1 pr-3 text-right">…</td>
                <td className="py-1 pr-3 text-right">…</td>
                <td className="py-1">in flight</td>
              </tr>
            ))}
            {rows.map((r) => {
              const failed = r.outcome.kind === 'failed' || r.outcome.kind === 'rejected' || r.outcome.kind === 'blocked'
              const aborted = r.outcome.kind === 'client_aborted'
              const rowClass = failed ? 'text-red-300' : aborted ? 'text-zinc-500 italic' : 'text-zinc-200'
              const tok = r.tokens_estimated ? '~' : ''
              const tim = r.timings_estimated ? '~' : ''
              const model = r.model_resolved ?? r.model_requested ?? '?'
              const stopClass = !failed && filledContext(r, models) ? 'text-red-300' : ''
              return (
                <tr key={r.id} className={`border-t border-zinc-800/60 ${rowClass}`}>
                  <td className="py-1 pr-3 tabular-nums">
                    <Link to={`/requests/${r.id}`} className="hover:underline">
                      {formatTime(r.ended_at)}
                    </Link>
                  </td>
                  {showBackend && <td className="py-1 pr-3 text-zinc-400">{BACKEND_NAMES[r.backend]}</td>}
                  <td className="py-1 pr-3" title={model}>
                    {truncate(model)}
                    {r.is_cloud && <span className="ink-cloud" title="routed to the vendor's cloud"> ☁</span>}
                  </td>
                  {failed ? (
                    <>
                      <td className="py-1 pr-3 text-right tabular-nums">{r.outcome.kind === 'rejected' && r.outcome.input_tokens !== null ? r.outcome.input_tokens : '-'}</td>
                      <td className="py-1 pr-3 text-right">-</td>
                      <td className="py-1 pr-3 text-right">-</td>
                      <td className="py-1 pr-3 text-right">-</td>
                    </>
                  ) : (
                    <>
                      <td className="py-1 pr-3 text-right tabular-nums">{r.prompt_tokens === null ? '?' : `${tok}${r.prompt_tokens}`}</td>
                      <td className="py-1 pr-3 text-right tabular-nums">{r.gen_tokens === null ? '?' : `${tok}${r.gen_tokens}`}</td>
                      <td className="py-1 pr-3 text-right tabular-nums" title={r.timings_estimated ? 'measured by the gateway, not the engine' : undefined}>
                        {r.ttft_ms === null ? '-' : `${tim}${formatMs(r.ttft_ms)}`}
                      </td>
                      <td className="py-1 pr-3 text-right tabular-nums">{r.tokens_per_sec === null ? '-' : `${tim}${formatTps(r.tokens_per_sec)}`}</td>
                    </>
                  )}
                  <td className={`py-1 ${stopClass}`} title={r.error_summary ?? undefined}>
                    {outcomeText(r)}
                  </td>
                </tr>
              )
            })}
            {rows.length === 0 && visibleInflight.length === 0 && (
              <tr>
                <td colSpan={showBackend ? 8 : 7} className="py-3 text-zinc-500">
                  No requests yet. Point a client at a gateway listener and they will appear here.
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </div>
  )
}
