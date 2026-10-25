import { useEffect, useState } from 'react'
import { NavLink } from 'react-router'
import { useLifetime, useStatus } from '../api/hooks'
import { BACKEND_NAMES, type Backend } from '../api/types'
import { formatClock, formatInt, formatUsd } from '../lib/format'
import { useLive } from '../store/live'

const PILL: Record<string, string> = {
  open: 'bg-emerald-500/15 text-emerald-300 ring-emerald-500/30',
  connecting: 'bg-amber-500/15 text-amber-300 ring-amber-500/30',
  closed: 'bg-red-500/15 text-red-300 ring-red-500/30',
}

function BackendPill({ backend }: { backend: Backend }) {
  const snapshot = useLive((s) => s.backends[backend])
  const status = useStatus()
  const fallback = status.data?.backends.find((b) => b.backend === backend)
  const enabled = fallback?.enabled ?? true
  const reachable = snapshot ? snapshot.reachable : fallback?.reachable ?? null
  const version = snapshot?.version ?? fallback?.version ?? null
  const error = snapshot?.error ?? fallback?.error ?? null
  const dot = !enabled ? 'ink-muted' : reachable === null ? 'ink-warning' : reachable ? 'ink-good' : 'ink-critical'
  const word = !enabled ? 'disabled' : reachable === null ? 'unknown' : reachable ? 'reachable' : 'unreachable'
  return (
    <span className="flex items-center gap-1.5 text-xs" title={error ?? undefined}>
      <span className={dot}>●</span>
      <span className="text-zinc-300">{BACKEND_NAMES[backend]}</span>
      <span className="font-mono text-zinc-500">
        {word}
        {version ? ` · v${version}` : ''}
      </span>
      {error && <span className="max-w-[16rem] truncate text-red-300">err: {error.slice(0, 60)}</span>}
    </span>
  )
}

export default function StatusBar() {
  const connection = useLive((s) => s.connection)
  const server = useLive((s) => s.server)
  const paused = useLive((s) => s.paused)
  const restartRequired = useLive((s) => s.restartRequired)
  const status = useStatus()
  const lifetime = useLifetime()
  const [now, setNow] = useState(() => new Date())
  useEffect(() => {
    const timer = setInterval(() => setNow(new Date()), 1000)
    return () => clearInterval(timer)
  }, [])

  const version = server?.version ?? status.data?.version
  const platform = server?.platform ?? status.data?.platform

  return (
    <header className="flex flex-wrap items-center gap-x-4 gap-y-1 border-b border-zinc-800 px-4 py-2">
      <span className="font-semibold tracking-tight text-zinc-50">WSDS Local AI Gateway</span>
      {version && (
        <span className="font-mono text-xs text-zinc-500">
          v{version}
          {platform ? ` · ${platform}` : ''}
        </span>
      )}
      <span className={`rounded-full px-2 py-0.5 text-xs ring-1 ${PILL[connection]}`}>live: {connection}</span>
      <BackendPill backend="lmstudio" />
      <BackendPill backend="ollama" />
      {paused && <span className="rounded bg-amber-500/15 px-1.5 py-0.5 text-xs font-semibold text-amber-300">PAUSED</span>}
      <NavLink
        to="/security"
        className={`rounded px-2 py-0.5 text-xs ring-1 ${
          (status.data?.unacked_alerts ?? 0) > 0 ? 'bg-red-500/15 text-red-300 ring-red-500/30' : 'text-zinc-500 ring-zinc-800'
        }`}
        title="unacknowledged alerts"
      >
        alerts {status.data ? formatInt(status.data.unacked_alerts) : '-'}
      </NavLink>
      {status.data && (status.data.dlp.cloud_requests_24h > 0 || status.data.dlp.cloud_spend_24h_usd > 0) && (
        <NavLink
          to="/cloud"
          className="text-xs ink-cloud"
          title="requests whose prompt left the machine in the last 24 hours, and what the cloud providers charged at list price"
        >
          ☁ {formatInt(status.data.dlp.cloud_requests_24h)} cloud / 24h
          {status.data.dlp.cloud_spend_24h_usd > 0 ? ` · ${formatUsd(status.data.dlp.cloud_spend_24h_usd)}` : ''}
        </NavLink>
      )}
      {restartRequired.length > 0 && (
        <NavLink to="/settings" className="rounded bg-amber-500/15 px-1.5 py-0.5 text-xs text-amber-300" title={restartRequired.join(', ')}>
          restart required: {restartRequired.join(', ')}
        </NavLink>
      )}
      {lifetime.data && (
        <span className="font-mono text-xs text-cyan-300/80">
          lifetime: {formatInt(lifetime.data.request_count)} reqs / {formatInt(lifetime.data.session_count)} sessions /{' '}
          {formatInt(lifetime.data.prompt_tokens)} prompt tok / {formatInt(lifetime.data.gen_tokens)} gen tok
        </span>
      )}
      {status.isError && <span className="text-xs text-red-300">{(status.error as Error).message}</span>}
      <span className="ml-auto font-mono text-sm tabular-nums text-zinc-400">{formatClock(now)}</span>
    </header>
  )
}
