import type { CostTable as Cost } from '../api/types'
import { formatInt, formatUsd } from '../lib/format'

/** Transposed like the TUIs: one column per frontier model, rows input / output / total. */
export default function CostTable({ cost }: { cost: Cost | undefined }) {
  if (!cost) return <p className="text-sm text-zinc-500">No cost data yet.</p>
  return (
    <div>
      <p className="mb-2 text-xs text-zinc-500">
        prompt={formatInt(cost.prompt_tokens)} tok / gen={formatInt(cost.gen_tokens)} tok at list prices, had they gone to a frontier API
      </p>
      <div className="overflow-auto">
        <table className="w-full font-mono text-sm">
          <thead className="text-xs text-zinc-500">
            <tr>
              <th className="pb-1 text-left font-normal" />
              {cost.columns.map((c) => (
                <th key={c} className="pb-1 pl-3 text-right font-normal whitespace-nowrap">
                  {c}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {(['input_usd', 'output_usd', 'total_usd'] as const).map((row) => (
              <tr key={row} className="border-t border-zinc-800/60">
                <td className="py-1 text-zinc-400">{row.replace('_usd', '')}</td>
                {cost.columns.map((c) => {
                  const entry = cost.costs[c]
                  return (
                    <td key={c} className={`py-1 pl-3 text-right tabular-nums ${row === 'total_usd' ? 'text-zinc-50' : ''}`}>
                      {entry ? formatUsd(entry[row]) : '(no rate)'}
                    </td>
                  )
                })}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  )
}
