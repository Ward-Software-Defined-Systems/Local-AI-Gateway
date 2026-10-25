import { BACKEND_NAMES, type Spend } from '../api/types'
import { formatInt, formatUsd } from '../lib/format'

/** What each cloud provider was asked and charged, at list price, over the session. */
export default function SpendTable({ spend }: { spend: Spend | undefined }) {
  if (!spend) return <p className="text-sm text-zinc-500">No spend data yet.</p>
  if (spend.rows.length === 0) return <p className="text-sm text-zinc-500">No completed cloud requests in this session.</p>
  const unpriced = spend.rows.reduce((n, r) => n + r.unpriced, 0)
  return (
    <div>
      <div className="overflow-auto">
        <table className="w-full font-mono text-sm">
          <thead className="text-xs text-zinc-500">
            <tr>
              <th className="pb-1 text-left font-normal">provider</th>
              <th className="pb-1 pl-3 text-right font-normal">requests</th>
              <th className="pb-1 pl-3 text-right font-normal">prompt</th>
              <th className="pb-1 pl-3 text-right font-normal">gen</th>
              <th className="pb-1 pl-3 text-right font-normal">spend</th>
            </tr>
          </thead>
          <tbody>
            {spend.rows.map((r) => (
              <tr key={r.backend} className="border-t border-zinc-800/60">
                <td className="py-1 text-zinc-300">{BACKEND_NAMES[r.backend]}</td>
                <td className="py-1 pl-3 text-right tabular-nums">{formatInt(r.requests)}</td>
                <td className="py-1 pl-3 text-right tabular-nums">{formatInt(r.prompt_tokens)}</td>
                <td className="py-1 pl-3 text-right tabular-nums">{formatInt(r.gen_tokens)}</td>
                <td className="py-1 pl-3 text-right tabular-nums" title={r.unpriced > 0 ? `${r.unpriced} request(s) with no rate in the pricing table` : undefined}>
                  {formatUsd(r.spend_usd)}
                  {r.unpriced > 0 ? '*' : ''}
                </td>
              </tr>
            ))}
            <tr className="border-t border-zinc-700">
              <td className="py-1 text-zinc-400">total</td>
              <td className="py-1 pl-3 text-right tabular-nums text-zinc-400">{formatInt(spend.rows.reduce((n, r) => n + r.requests, 0))}</td>
              <td className="py-1 pl-3 text-right tabular-nums text-zinc-400">{formatInt(spend.rows.reduce((n, r) => n + r.prompt_tokens, 0))}</td>
              <td className="py-1 pl-3 text-right tabular-nums text-zinc-400">{formatInt(spend.rows.reduce((n, r) => n + r.gen_tokens, 0))}</td>
              <td className="py-1 pl-3 text-right tabular-nums text-zinc-50">{formatUsd(spend.total_usd)}</td>
            </tr>
          </tbody>
        </table>
      </div>
      <p className="mt-2 text-xs text-zinc-500">
        List prices from pricing.toml, flat per token: cache discounts and tiers are not applied, so this is an upper bound.
        {unpriced > 0 ? ` * ${unpriced} completed request(s) used a model with no rate and count nothing.` : ''}
      </p>
    </div>
  )
}
