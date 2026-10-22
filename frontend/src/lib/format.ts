/** `3d 04:05:06`, `04:05:06` or `05:06` for a duration in seconds. */
export function formatUptime(totalSeconds: number): string {
  const s = Math.max(0, Math.floor(totalSeconds))
  const days = Math.floor(s / 86_400)
  const hours = Math.floor((s % 86_400) / 3600)
  const minutes = Math.floor((s % 3600) / 60)
  const seconds = s % 60
  const pad = (n: number) => n.toString().padStart(2, '0')
  const hms = `${pad(hours)}:${pad(minutes)}:${pad(seconds)}`
  return days > 0 ? `${days}d ${hms}` : hms
}

/** Local `HH:MM:SS`, as the TUIs showed it. */
export function formatClock(date: Date): string {
  const pad = (n: number) => n.toString().padStart(2, '0')
  return `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`
}

export function formatTime(iso: string): string {
  return formatClock(new Date(iso))
}

/** Decimal units, as the TUIs and Activity Monitor show them: `38.2 GB`, `512 MB`. */
export function formatBytes(b: number | null | undefined): string {
  if (b === null || b === undefined) return 'n/a'
  if (b >= 1e9) return `${(b / 1e9).toFixed(1)} GB`
  if (b >= 1e6) return `${Math.round(b / 1e6)} MB`
  if (b >= 1e3) return `${Math.round(b / 1e3)} KB`
  return `${b} B`
}

/** `38.2/137.4 GB`: one unit for both, chosen from the total. */
export function formatBytesRatio(used: number, total: number): string {
  const [div, unit, digits] = total >= 1e9 ? [1e9, 'GB', 1] : total >= 1e6 ? [1e6, 'MB', 0] : total >= 1e3 ? [1e3, 'KB', 0] : [1, 'B', 0]
  return `${(used / div).toFixed(digits)}/${(total / div).toFixed(digits)} ${unit}`
}

/** Compact counts for axes and tiles: 1,284 → 1.3K, 2,500,000 → 2.5M. */
export function formatCompact(n: number): string {
  if (!Number.isFinite(n)) return '-'
  const abs = Math.abs(n)
  if (abs >= 1e9) return `${(n / 1e9).toFixed(1)}B`
  if (abs >= 1e6) return `${(n / 1e6).toFixed(1)}M`
  if (abs >= 10_000) return `${(n / 1e3).toFixed(0)}K`
  if (abs >= 1e3) return `${(n / 1e3).toFixed(1)}K`
  return Number.isInteger(n) ? n.toString() : n.toFixed(1)
}

export function formatInt(n: number | null | undefined): string {
  if (n === null || n === undefined) return '-'
  return n.toLocaleString()
}

export function formatTps(v: number | null | undefined): string {
  if (v === null || v === undefined || !Number.isFinite(v)) return '-'
  return v.toFixed(1)
}

export function formatMs(v: number | null | undefined): string {
  if (v === null || v === undefined || !Number.isFinite(v)) return '-'
  return `${Math.round(v)}ms`
}

export function formatSeconds(ms: number): string {
  return `${(ms / 1000).toFixed(1)}s`
}

/** `$0.0036`, the TUIs' four decimals. */
export function formatUsd(v: number | null | undefined): string {
  if (v === null || v === undefined || !Number.isFinite(v)) return '(no rate)'
  return `$${v.toFixed(4)}`
}

export function formatPct(v: number | null | undefined, digits = 0): string {
  if (v === null || v === undefined || !Number.isFinite(v)) return 'n/a'
  return `${v.toFixed(digits)}%`
}

/** Cut to `max` characters with an ellipsis, as the TUI did at 28. */
export function truncate(s: string, max = 28): string {
  return s.length <= max ? s : `${s.slice(0, max - 1)}…`
}

/** `in 4m`, `in 2h 10m`, `expired`, or `never` for the far-future expiry Ollama reports for keep_alive -1. */
export function formatCountdown(iso: string | null, now: Date): string {
  if (!iso) return '-'
  const diff = Math.round((new Date(iso).getTime() - now.getTime()) / 1000)
  if (diff <= 0) return 'expired'
  if (diff > 365 * 86_400) return 'never'
  if (diff < 60) return `in ${diff}s`
  if (diff < 3600) return `in ${Math.round(diff / 60)}m`
  const h = Math.floor(diff / 3600)
  const m = Math.round((diff % 3600) / 60)
  return `in ${h}h ${m}m`
}

/** Local `YYYY-MM-DD HH:MM:SS`, for rows that may be older than today. */
export function formatDateTime(iso: string): string {
  const d = new Date(iso)
  const pad = (n: number) => n.toString().padStart(2, '0')
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${formatClock(d)}`
}
