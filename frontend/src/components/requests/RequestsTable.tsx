import { Link } from 'react-router'
import { BACKEND_NAMES, type RequestSummary } from '../../api/types'
import { formatBytes, formatInt, formatSeconds, formatTime, truncate } from '../../lib/format'

function outcomeBadge(r: RequestSummary): { text: string; cls: string } {
  const o = r.outcome
  switch (o.kind) {
    case 'completed':
      return { text: `${r.status ?? ''} ok`, cls: 'ink-good' }
    case 'failed':
      return { text: `${o.status} ${o.source === 'gateway' ? 'gateway' : 'failed'}`, cls: 'ink-critical' }
    case 'rejected':
      return { text: 'rejected', cls: 'ink-critical' }
    case 'blocked':
      return { text: 'blocked', cls: 'ink-critical' }
    case 'client_aborted':
      return { text: 'aborted', cls: 'ink-muted' }
  }
}

export default function RequestsTable({ rows, selected }: { rows: RequestSummary[]; selected?: string }) {
  if (rows.length === 0) return <p className="py-4 text-sm text-zinc-500">No requests match.</p>
  return (
    <table className="w-full font-mono text-xs">
      <thead className="sticky top-0 bg-zinc-900 text-left text-zinc-500">
        <tr>
          <th className="pb-1 pr-3 font-normal">time</th>
          <th className="pb-1 pr-3 font-normal">backend</th>
          <th className="pb-1 pr-3 font-normal">request</th>
          <th className="pb-1 pr-3 font-normal">model</th>
          <th className="pb-1 pr-3 font-normal">client</th>
          <th className="pb-1 pr-3 font-normal">outcome</th>
          <th className="pb-1 pr-3 text-right font-normal">prompt</th>
          <th className="pb-1 pr-3 text-right font-normal">gen</th>
          <th className="pb-1 pr-3 text-right font-normal">time</th>
          <th className="pb-1 pr-3 text-right font-normal">bytes</th>
          <th className="pb-1 font-normal" />
        </tr>
      </thead>
      <tbody>
        {rows.map((r) => {
          const badge = outcomeBadge(r)
          const model = r.model_resolved ?? r.model_requested
          const tok = r.tokens_estimated ? '~' : ''
          return (
            <tr key={r.id} className={`border-t border-zinc-800/60 ${selected === r.id ? 'bg-zinc-800/60' : 'hover:bg-zinc-800/30'}`}>
              <td className="py-1 pr-3 whitespace-nowrap tabular-nums">
                <Link to={`/requests/${r.id}`} className="text-zinc-200 hover:underline">
                  {formatTime(r.started_at)}
                </Link>
              </td>
              <td className="py-1 pr-3 text-zinc-400">{BACKEND_NAMES[r.backend]}</td>
              <td className="py-1 pr-3 whitespace-nowrap" title={`${r.method} ${r.path}${r.query ? `?${r.query}` : ''} · ${r.api_format}`}>
                <span className="text-zinc-500">{r.method}</span> {truncate(r.path, 32)}
                {r.kind === 'upgrade' && <span className="ml-1 text-zinc-500">ws</span>}
                {r.streamed && <span className="ml-1 text-zinc-500">stream</span>}
              </td>
              <td className="py-1 pr-3" title={model ?? undefined}>
                {model ? truncate(model, 26) : <span className="text-zinc-600">-</span>}
                {r.is_cloud && <span className="ink-cloud"> ☁</span>}
              </td>
              <td className="py-1 pr-3 text-zinc-400" title={r.user_agent ?? undefined}>
                {r.client_ip}
              </td>
              <td className={`py-1 pr-3 whitespace-nowrap ${badge.cls}`} title={r.error_summary ?? undefined}>
                {badge.text}
              </td>
              <td className="py-1 pr-3 text-right tabular-nums text-zinc-300" title={r.tokens_estimated ? 'estimated: the server sent no usage' : undefined}>
                {r.prompt_tokens === null ? <span className="text-zinc-600">-</span> : `${tok}${formatInt(r.prompt_tokens)}`}
              </td>
              <td className="py-1 pr-3 text-right tabular-nums text-zinc-300" title={r.tokens_estimated ? 'estimated: the server sent no usage' : undefined}>
                {r.gen_tokens === null ? <span className="text-zinc-600">-</span> : `${tok}${formatInt(r.gen_tokens)}`}
              </td>
              <td className="py-1 pr-3 text-right tabular-nums text-zinc-300">{formatSeconds(r.total_ms)}</td>
              <td className="py-1 pr-3 text-right tabular-nums text-zinc-400">{formatBytes(r.request_bytes + r.response_bytes)}</td>
              <td className="py-1 text-zinc-500">
                {r.finding_count > 0 && (
                  <span className={r.max_severity === 'critical' || r.max_severity === 'high' ? 'ink-critical' : 'ink-warning'} title={`${r.finding_count} finding(s)`}>
                    ⚑{r.finding_count}
                  </span>
                )}
                {(r.has_request_body || r.has_response_body) && <span title="bodies stored"> ▤</span>}
              </td>
            </tr>
          )
        })}
      </tbody>
    </table>
  )
}
