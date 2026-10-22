import { API_FORMATS, BACKEND_NAMES, OUTCOMES, type Backend } from '../../api/types'

export interface FilterState {
  backend: Backend | ''
  outcome: string
  format: string
  model: string
  client: string
  q: string
  cloud: boolean
  hasFindings: boolean
  includePassthrough: boolean
  range: string
}

export const EMPTY_FILTERS: FilterState = {
  backend: '',
  outcome: '',
  format: '',
  model: '',
  client: '',
  q: '',
  cloud: false,
  hasFindings: false,
  includePassthrough: false,
  range: 'all',
}

export const REQUEST_RANGES: { key: string; label: string; seconds: number | null }[] = [
  { key: 'all', label: 'all time', seconds: null },
  { key: '1h', label: 'last hour', seconds: 3600 },
  { key: '24h', label: 'last 24 hours', seconds: 86_400 },
  { key: '7d', label: 'last 7 days', seconds: 7 * 86_400 },
]

const select = 'rounded border border-zinc-700 bg-zinc-900 px-2 py-1 text-xs text-zinc-200'
const input = 'rounded border border-zinc-700 bg-zinc-900 px-2 py-1 text-xs text-zinc-200 placeholder:text-zinc-600'

export default function Filters({ value, onChange }: { value: FilterState; onChange: (next: FilterState) => void }) {
  const set = <K extends keyof FilterState>(key: K, v: FilterState[K]) => onChange({ ...value, [key]: v })
  return (
    <div className="flex flex-wrap items-center gap-2">
      <input className={`${input} w-56`} placeholder="search bodies (FTS)…" value={value.q} onChange={(e) => set('q', e.target.value)} aria-label="search bodies" />
      <select className={select} value={value.range} onChange={(e) => set('range', e.target.value)} aria-label="time range">
        {REQUEST_RANGES.map((r) => (
          <option key={r.key} value={r.key}>
            {r.label}
          </option>
        ))}
      </select>
      <select className={select} value={value.backend} onChange={(e) => set('backend', e.target.value as Backend | '')} aria-label="backend">
        <option value="">both backends</option>
        {(Object.keys(BACKEND_NAMES) as Backend[]).map((b) => (
          <option key={b} value={b}>
            {BACKEND_NAMES[b]}
          </option>
        ))}
      </select>
      <select className={select} value={value.outcome} onChange={(e) => set('outcome', e.target.value)} aria-label="outcome">
        <option value="">any outcome</option>
        {OUTCOMES.map((o) => (
          <option key={o} value={o}>
            {o.replace('_', ' ')}
          </option>
        ))}
      </select>
      <select className={select} value={value.format} onChange={(e) => set('format', e.target.value)} aria-label="api format">
        <option value="">any format</option>
        {API_FORMATS.map((f) => (
          <option key={f} value={f}>
            {f}
          </option>
        ))}
      </select>
      <input className={`${input} w-40`} placeholder="model" value={value.model} onChange={(e) => set('model', e.target.value)} aria-label="model" />
      <input className={`${input} w-32`} placeholder="client IP" value={value.client} onChange={(e) => set('client', e.target.value)} aria-label="client" />
      <label className="flex items-center gap-1 text-xs text-zinc-400">
        <input type="checkbox" checked={value.cloud} onChange={(e) => set('cloud', e.target.checked)} /> cloud only
      </label>
      <label className="flex items-center gap-1 text-xs text-zinc-400">
        <input type="checkbox" checked={value.hasFindings} onChange={(e) => set('hasFindings', e.target.checked)} /> with findings
      </label>
      <label className="flex items-center gap-1 text-xs text-zinc-400">
        <input type="checkbox" checked={value.includePassthrough} onChange={(e) => set('includePassthrough', e.target.checked)} /> passthrough
      </label>
      <button type="button" className="rounded px-2 py-1 text-xs text-zinc-400 hover:bg-zinc-800" onClick={() => onChange(EMPTY_FILTERS)}>
        clear
      </button>
    </div>
  )
}
