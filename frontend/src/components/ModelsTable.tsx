import { useEffect, useState } from 'react'
import { BACKEND_NAMES, type Backend, type ModelInfo } from '../api/types'
import { stateClass } from '../lib/colours'
import { formatBytes, formatCountdown, formatInt, truncate } from '../lib/format'

interface Props {
  models: ModelInfo[]
  /** Shown on the Overview, where both backends share one table. */
  showBackend?: boolean
  lastModel?: Partial<Record<Backend, string>>
}

export default function ModelsTable({ models, showBackend = false, lastModel = {} }: Props) {
  const [now, setNow] = useState(() => new Date())
  const hasOllama = models.some((m) => m.backend === 'ollama')
  useEffect(() => {
    if (!hasOllama) return
    const timer = setInterval(() => setNow(new Date()), 5000)
    return () => clearInterval(timer)
  }, [hasOllama])
  if (models.length === 0) return <p className="text-sm text-zinc-500">No models reported.</p>
  return (
    <div className="max-h-72 overflow-auto">
      <table className="w-full font-mono text-sm">
        <thead className="sticky top-0 bg-zinc-900 text-left text-xs text-zinc-500">
          <tr>
            <th className="pb-1 pr-2 font-normal" />
            {showBackend && <th className="pb-1 pr-3 font-normal">backend</th>}
            <th className="pb-1 pr-3 font-normal">id</th>
            <th className="pb-1 pr-3 font-normal">type</th>
            <th className="pb-1 pr-3 font-normal">compat</th>
            <th className="pb-1 pr-3 font-normal">quant</th>
            <th className="pb-1 pr-3 text-right font-normal">ctx</th>
            <th className="pb-1 pr-3 font-normal">state</th>
            {hasOllama && <th className="pb-1 pr-3 text-right font-normal">size</th>}
            {hasOllama && <th className="pb-1 font-normal">expires</th>}
          </tr>
        </thead>
        <tbody>
          {models.map((m) => {
            const marked = lastModel[m.backend] === m.id
            return (
              <tr key={`${m.backend}/${m.id}`} className={`border-t border-zinc-800/60 ${marked ? 'font-semibold text-cyan-300' : ''}`}>
                <td className="py-1 pr-2 text-cyan-300">{marked ? '▸' : ''}</td>
                {showBackend && <td className="py-1 pr-3 text-zinc-400">{BACKEND_NAMES[m.backend]}</td>}
                <td className="py-1 pr-3" title={m.id}>
                  {truncate(m.id, 36)}
                </td>
                <td className="py-1 pr-3 text-zinc-400">{m.kind ?? '-'}</td>
                <td className="py-1 pr-3 text-zinc-400">{m.compatibility_type ?? '-'}</td>
                <td className="py-1 pr-3 text-zinc-400">{m.quantization ?? '-'}</td>
                <td className="py-1 pr-3 text-right tabular-nums text-zinc-400" title={m.loaded_context_length ? `loaded ${formatInt(m.loaded_context_length)}` : undefined}>
                  {m.max_context_length ? formatInt(m.max_context_length) : '-'}
                </td>
                <td className={`py-1 pr-3 ${stateClass(m.state)}`}>{m.state}</td>
                {hasOllama && (
                  <td className="py-1 pr-3 text-right tabular-nums text-zinc-400">
                    {m.backend === 'ollama' && m.state !== 'cloud' ? formatBytes(m.size_vram_bytes ?? m.size_bytes) : ''}
                  </td>
                )}
                {hasOllama && <td className="py-1 text-zinc-400">{m.backend === 'ollama' && m.state === 'loaded' ? formatCountdown(m.expires_at, now) : ''}</td>}
              </tr>
            )
          })}
        </tbody>
      </table>
    </div>
  )
}
