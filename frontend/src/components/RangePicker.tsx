import { RANGES, useRange } from '../lib/range'

/** One row, above the charts it scopes. */
export default function RangePicker() {
  const key = useRange((s) => s.key)
  const set = useRange((s) => s.set)
  return (
    <div className="flex items-center gap-1 text-xs">
      <span className="mr-1 text-zinc-500">range</span>
      {RANGES.map((r) => (
        <button
          key={r.key}
          type="button"
          onClick={() => set(r.key)}
          aria-pressed={key === r.key}
          className={`rounded px-2 py-1 ${
            key === r.key ? 'bg-zinc-700 text-zinc-50' : 'text-zinc-400 hover:bg-zinc-800 hover:text-zinc-200'
          }`}
        >
          {r.label}
        </button>
      ))}
    </div>
  )
}
