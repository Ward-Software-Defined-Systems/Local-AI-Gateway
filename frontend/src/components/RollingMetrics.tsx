import type { RollingMetrics as Rolling, WindowStats } from '../api/types'
import { formatInt, formatMs, formatTps } from '../lib/format'

const COLUMNS: [keyof Pick<Rolling, 'one_minute' | 'five_minute' | 'fifteen_minute' | 'session'>, string][] = [
  ['one_minute', '1m'],
  ['five_minute', '5m'],
  ['fifteen_minute', '15m'],
  ['session', 'session'],
]

const ROWS: [string, (w: WindowStats) => string][] = [
  ['requests', (w) => formatInt(w.request_count)],
  ['prompt tok', (w) => formatInt(w.prompt_tokens)],
  ['gen tok', (w) => formatInt(w.gen_tokens)],
  ['mean tok/s', (w) => formatTps(w.mean_tps)],
  ['p95 tok/s', (w) => formatTps(w.p95_tps)],
  ['mean TTFT', (w) => formatMs(w.mean_ttft_ms)],
]

export default function RollingMetrics({ metrics }: { metrics: Rolling | null | undefined }) {
  if (!metrics) return <p className="text-sm text-zinc-500">No metrics yet.</p>
  return (
    <table className="w-full font-mono text-sm">
      <thead className="text-xs text-zinc-500">
        <tr>
          <th className="pb-1 text-left font-normal" />
          {COLUMNS.map(([, label]) => (
            <th key={label} className="pb-1 text-right font-normal">
              {label}
            </th>
          ))}
        </tr>
      </thead>
      <tbody>
        {ROWS.map(([label, pick]) => (
          <tr key={label} className="border-t border-zinc-800/60">
            <td className="py-1 text-zinc-400">{label}</td>
            {COLUMNS.map(([key]) => (
              <td key={key} className="py-1 text-right tabular-nums">
                {pick(metrics[key])}
              </td>
            ))}
          </tr>
        ))}
      </tbody>
    </table>
  )
}
