import type { Finding } from '../../api/types'
import { severityClass } from '../../lib/colours'

export default function FindingsTable({ findings }: { findings: Finding[] }) {
  if (findings.length === 0) return <p className="text-sm text-zinc-500">No DLP findings on this request.</p>
  return (
    <table className="w-full font-mono text-xs">
      <thead className="text-left text-zinc-500">
        <tr>
          <th className="pb-1 pr-3 font-normal">severity</th>
          <th className="pb-1 pr-3 font-normal">rule</th>
          <th className="pb-1 pr-3 font-normal">where</th>
          <th className="pb-1 pr-3 font-normal">location</th>
          <th className="pb-1 pr-3 font-normal">preview</th>
          <th className="pb-1 pr-3 font-normal">action</th>
          <th className="pb-1 font-normal">fingerprint</th>
        </tr>
      </thead>
      <tbody>
        {findings.map((f, i) => (
          <tr key={i} className="border-t border-zinc-800/60">
            <td className={`py-1 pr-3 ${severityClass(f.severity)}`}>{f.severity}</td>
            <td className="py-1 pr-3">{f.rule_id}</td>
            <td className="py-1 pr-3 text-zinc-400">{f.direction}</td>
            <td className="py-1 pr-3 text-zinc-400">{f.location}</td>
            <td className="py-1 pr-3">{f.preview}</td>
            <td className="py-1 pr-3 text-zinc-400">{f.action}</td>
            <td className="py-1 text-zinc-500">{f.fingerprint.slice(0, 12)}…</td>
          </tr>
        ))}
      </tbody>
    </table>
  )
}
