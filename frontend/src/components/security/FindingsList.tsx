import { useMemo, useState } from 'react'
import { Link } from 'react-router'
import { useFindings, type FindingListParams } from '../../api/hooks'
import { BACKEND_NAMES, SEVERITIES, type Backend } from '../../api/types'
import { severityClass } from '../../lib/colours'
import { formatDateTime, truncate } from '../../lib/format'
import { REQUEST_RANGES } from '../requests/Filters'

const select = 'rounded border border-zinc-700 bg-zinc-900 px-2 py-1 text-xs text-zinc-200'
const input = 'rounded border border-zinc-700 bg-zinc-900 px-2 py-1 text-xs text-zinc-200 placeholder:text-zinc-600'
const button = 'rounded border border-zinc-700 px-2 py-0.5 text-xs text-zinc-300 hover:bg-zinc-800 disabled:opacity-50'

interface Filter {
  rule: string
  severity: string
  backend: Backend | ''
  direction: string
  range: string
}

const EMPTY: Filter = { rule: '', severity: '', backend: '', direction: '', range: '24h' }

function toParams(f: Filter): FindingListParams {
  const preset = REQUEST_RANGES.find((r) => r.key === f.range)
  const from = preset?.seconds ? new Date(Math.floor(Date.now() / 60_000) * 60_000 - preset.seconds * 1000).toISOString() : undefined
  return {
    rule: f.rule.trim() || undefined,
    severity: f.severity || undefined,
    backend: f.backend || undefined,
    direction: f.direction || undefined,
    from,
  }
}

/** Every finding the store holds, newest first: never a value, only the rule, the place and a masked preview. */
export default function FindingsList() {
  const [filter, setFilter] = useState<Filter>(EMPTY)
  const params = useMemo(() => toParams(filter), [filter])
  const findings = useFindings(params)
  const rows = useMemo(() => findings.data?.pages.flatMap((p) => p.items) ?? [], [findings.data])
  const set = <K extends keyof Filter>(key: K, v: Filter[K]) => setFilter({ ...filter, [key]: v })

  return (
    <div>
      <div className="mb-2 flex flex-wrap items-center gap-2">
        <input className={`${input} w-44`} placeholder="rule id" value={filter.rule} onChange={(e) => set('rule', e.target.value)} aria-label="rule" />
        <select className={select} value={filter.severity} onChange={(e) => set('severity', e.target.value)} aria-label="severity">
          <option value="">any severity</option>
          {SEVERITIES.map((s) => (
            <option key={s} value={s}>
              {s}
            </option>
          ))}
        </select>
        <select className={select} value={filter.backend} onChange={(e) => set('backend', e.target.value as Backend | '')} aria-label="backend">
          <option value="">all backends</option>
          {(Object.keys(BACKEND_NAMES) as Backend[]).map((b) => (
            <option key={b} value={b}>
              {BACKEND_NAMES[b]}
            </option>
          ))}
        </select>
        <select className={select} value={filter.direction} onChange={(e) => set('direction', e.target.value)} aria-label="direction">
          <option value="">requests and replies</option>
          <option value="request">requests</option>
          <option value="response">replies</option>
        </select>
        <select className={select} value={filter.range} onChange={(e) => set('range', e.target.value)} aria-label="time range">
          {REQUEST_RANGES.map((r) => (
            <option key={r.key} value={r.key}>
              {r.label}
            </option>
          ))}
        </select>
      </div>
      {findings.isError && <p className="text-sm text-red-300">{(findings.error as Error).message}</p>}
      {rows.length === 0 ? (
        <p className="text-sm text-zinc-500">No findings match.</p>
      ) : (
        <div className={`max-h-96 overflow-auto ${findings.isFetching && !findings.isFetchingNextPage ? 'opacity-70' : ''}`}>
          <table className="w-full font-mono text-xs">
            <thead className="sticky top-0 bg-zinc-900 text-left text-zinc-500">
              <tr>
                <th className="pb-1 pr-3 font-normal">time</th>
                <th className="pb-1 pr-3 font-normal">severity</th>
                <th className="pb-1 pr-3 font-normal">rule</th>
                <th className="pb-1 pr-3 font-normal">backend</th>
                <th className="pb-1 pr-3 font-normal">model</th>
                <th className="pb-1 pr-3 font-normal">where</th>
                <th className="pb-1 pr-3 font-normal">location</th>
                <th className="pb-1 pr-3 font-normal">preview</th>
                <th className="pb-1 pr-3 font-normal">action</th>
                <th className="pb-1 font-normal">request</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((f) => (
                <tr key={f.id} className="border-t border-zinc-800/60 hover:bg-zinc-800/30">
                  <td className="py-1 pr-3 whitespace-nowrap tabular-nums">{formatDateTime(f.created_at)}</td>
                  <td className={`py-1 pr-3 ${severityClass(f.severity)}`}>{f.severity}</td>
                  <td className="py-1 pr-3">{f.rule_id}</td>
                  <td className="py-1 pr-3 text-zinc-400">{BACKEND_NAMES[f.backend]}</td>
                  <td className="py-1 pr-3" title={f.model ?? undefined}>
                    {f.model ? truncate(f.model, 24) : <span className="text-zinc-600">-</span>}
                    {f.is_cloud && <span className="ink-cloud"> ☁</span>}
                  </td>
                  <td className="py-1 pr-3 text-zinc-400">{f.direction}</td>
                  <td className="py-1 pr-3 text-zinc-400" title={f.location}>
                    {truncate(f.location, 28)}
                  </td>
                  <td className="py-1 pr-3">{f.preview}</td>
                  <td className={`py-1 pr-3 ${f.action === 'block' ? 'ink-critical' : 'text-zinc-400'}`}>{f.action}</td>
                  <td className="py-1">
                    <Link to={`/requests/${f.request_id}`} className="text-zinc-300 hover:underline">
                      open
                    </Link>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      <div className="mt-2 flex items-center gap-3 text-xs text-zinc-500">
        <span>
          {rows.length} loaded{findings.hasNextPage ? ', more available' : ''}
        </span>
        {findings.hasNextPage && (
          <button type="button" className={button} onClick={() => void findings.fetchNextPage()} disabled={findings.isFetchingNextPage}>
            {findings.isFetchingNextPage ? 'loading…' : 'load more'}
          </button>
        )}
      </div>
    </div>
  )
}
