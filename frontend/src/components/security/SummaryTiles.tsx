import { useStatus } from '../../api/hooks'
import { formatInt } from '../../lib/format'

function Tile({ label, value, cls = 'text-zinc-100', hint }: { label: string; value: string; cls?: string; hint?: string }) {
  return (
    <div className="rounded border border-zinc-800 bg-zinc-900 px-3 py-2" title={hint}>
      <div className="text-[11px] uppercase tracking-wider text-zinc-500">{label}</div>
      <div className={`font-mono text-xl tabular-nums ${cls}`}>{value}</div>
    </div>
  )
}

/** The Security tab's headline numbers: the last 24 hours of findings and egress, and the policy's size. */
export default function SummaryTiles() {
  const status = useStatus()
  const d = status.data?.dlp
  const unacked = status.data?.unacked_alerts ?? 0
  return (
    <div className="grid grid-cols-2 gap-2 sm:grid-cols-3 lg:grid-cols-6">
      <Tile label="findings · 24h" value={d ? formatInt(d.findings_24h) : '-'} />
      <Tile label="blocked · 24h" value={d ? formatInt(d.blocked_24h) : '-'} cls={d && d.blocked_24h > 0 ? 'ink-critical' : 'text-zinc-100'} />
      <Tile
        label="cloud requests · 24h"
        value={d ? formatInt(d.cloud_requests_24h) : '-'}
        cls={d && d.cloud_requests_24h > 0 ? 'ink-cloud' : 'text-zinc-100'}
        hint="inference requests routed to an Ollama cloud model; their prompts left this machine"
      />
      <Tile label="unacknowledged alerts" value={status.data ? formatInt(unacked) : '-'} cls={unacked > 0 ? 'ink-critical' : 'text-zinc-100'} />
      <Tile
        label="rules enabled"
        value={d ? `${d.rules_enabled}/${d.rules_total}` : '-'}
        cls={d && !d.enabled ? 'ink-critical' : 'text-zinc-100'}
        hint={d && !d.enabled ? 'dlp.enabled = false: nothing is scanned' : `${d?.custom_rules ?? 0} custom, ${d?.allowlist_entries ?? 0} allowlist entries`}
      />
      <Tile label="known secrets" value={d ? formatInt(d.known_secrets) : '-'} hint="values from known-secrets.toml, matched in every encoding" />
    </div>
  )
}
