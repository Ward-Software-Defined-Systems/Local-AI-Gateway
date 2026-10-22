import { useStatus } from '../../api/hooks'
import { BACKEND_NAMES } from '../../api/types'
import { formatBytes, formatInt, formatUptime } from '../../lib/format'

function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <>
      <dt className="text-zinc-500">{label}</dt>
      <dd className="break-all">{children}</dd>
    </>
  )
}

/** Where this gateway runs from and what it fronts; everything here changes only on restart. */
export default function RuntimePanel() {
  const status = useStatus()
  const s = status.data
  if (status.isError) return <p className="text-sm text-red-300">{(status.error as Error).message}</p>
  if (!s) return <p className="text-sm text-zinc-500">Loading…</p>
  return (
    <div className="grid gap-4 lg:grid-cols-2">
      <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 font-mono text-xs">
        <Row label="version">
          {s.version} · {s.platform}
        </Row>
        <Row label="uptime">
          {formatUptime(s.uptime_s)}
          {s.session_id !== null ? ` · session ${s.session_id}` : ''}
        </Row>
        <Row label="dashboard">
          {s.dashboard.listen} · {s.dashboard.tls ? 'https' : 'http'} · auth {s.dashboard.auth ? 'on' : 'off'}
        </Row>
        <Row label="listeners">
          {s.listeners.length === 0 ? (
            <span className="text-zinc-500">none</span>
          ) : (
            <ul>
              {s.listeners.map((l) => (
                <li key={`${l.backend}-${l.listen}`}>
                  {l.listen} → {l.upstream} <span className="text-zinc-500">({BACKEND_NAMES[l.backend]})</span>
                </li>
              ))}
            </ul>
          )}
        </Row>
        <Row label="in flight">{formatInt(s.inflight)}</Row>
        <Row label="database">
          {s.db ? (
            <>
              {formatBytes(s.db.size_bytes)} · {formatInt(s.db.bodies)} bodies · retained {s.db.retention_days} days
            </>
          ) : (
            <span className="text-zinc-500">store disabled</span>
          )}
        </Row>
      </dl>
      <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 font-mono text-xs">
        <Row label="data dir">{s.paths.data_dir}</Row>
        <Row label="config">
          {s.paths.config_file}
          {!s.paths.config_exists && <span className="ml-2 text-zinc-500">(missing: running on defaults)</span>}
        </Row>
        <Row label="database file">{s.paths.db_file}</Row>
        <Row label="token file">{s.paths.token_file}</Row>
        <Row label="known secrets">{s.paths.known_secrets_file}</Row>
        <Row label="logs">{s.paths.logs_dir}</Row>
      </dl>
      <p className="text-xs text-zinc-500 lg:col-span-2">
        The access token is never shown here. Print the dashboard URL with <code>local-ai-gateway --print-token</code>; to rotate the
        token, delete the token file and restart.
      </p>
    </div>
  )
}
