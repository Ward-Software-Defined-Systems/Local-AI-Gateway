import type { Backend, HardwareSample, Platform, ProcStats } from '../api/types'
import { aneClass, pctClass } from '../lib/colours'
import { formatBytes, formatBytesRatio, formatPct } from '../lib/format'

interface Props {
  sample: HardwareSample | null
  platform: Platform | undefined
  /** Only this backend's process stats; the Overview shows both. */
  backend?: Backend
}

function Proc({ label, stats }: { label: string; stats: ProcStats }) {
  if (stats.process_count === 0) return <span className="text-zinc-500">{label}: no process detected</span>
  return (
    <span>
      <span className="text-zinc-500">{label}</span> cpu <span className={pctClass(stats.cpu_pct)}>{formatPct(stats.cpu_pct)}</span> rss{' '}
      {formatBytes(stats.rss_bytes)} {stats.process_count} proc{stats.process_count === 1 ? '' : 's'}
    </span>
  )
}

/** The TUIs' one-line hardware panel. */
export default function HardwareLine({ sample, platform, backend }: Props) {
  if (!sample) return <p className="text-sm text-zinc-500">No hardware sample yet.</p>
  const sep = <span className="text-zinc-700"> │ </span>
  return (
    <p className="font-mono text-sm tabular-nums text-zinc-300">
      cpu <span className={pctClass(sample.system_cpu_pct)}>{formatPct(sample.system_cpu_pct)}</span> mem{' '}
      {formatBytesRatio(sample.mem_used_bytes, sample.mem_total_bytes)} ({formatBytes(sample.mem_available_bytes)} free)
      {(backend === undefined || backend === 'lmstudio') && (
        <>
          {sep}
          <Proc label="lms" stats={sample.lmstudio} />
        </>
      )}
      {(backend === undefined || backend === 'ollama') && (
        <>
          {sep}
          <Proc label="ollama" stats={sample.ollama} />
        </>
      )}
      {sep}gpu <span className={pctClass(sample.gpu_util_pct)}>{formatPct(sample.gpu_util_pct)}</span>
      {platform === 'linux' ? (
        <>
          {' '}
          vram {formatBytes(sample.gpu_mem_used_bytes)}
        </>
      ) : (
        <>
          {' '}
          gpu mem {formatBytes(sample.gpu_mem_used_bytes)}
          {sep}ane <span className={aneClass(sample.ane_power_mw)}>{sample.ane_power_mw === null ? 'n/a' : `${Math.round(sample.ane_power_mw)} mW`}</span>
        </>
      )}
    </p>
  )
}
