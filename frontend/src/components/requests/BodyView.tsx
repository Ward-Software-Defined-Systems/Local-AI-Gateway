import { useState } from 'react'
import type { ApiFormat } from '../../api/types'
import type { BodyText } from '../../api/hooks'
import { formatBytes } from '../../lib/format'
import { prettyJson, reassemble } from '../../lib/sse'

interface Props {
  body: BodyText | null | undefined
  loading: boolean
  format: ApiFormat
  direction: 'request' | 'response'
  streamed: boolean
}

type Mode = 'text' | 'pretty' | 'raw'

export default function BodyView({ body, loading, format, direction, streamed }: Props) {
  const [mode, setMode] = useState<Mode | null>(null)
  if (loading) return <p className="text-sm text-zinc-500">Loading body…</p>
  if (body === null || body === undefined) return <p className="text-sm text-zinc-500">No {direction} body stored (capture is off, the body expired, or there was none).</p>
  const assembled = direction === 'response' && streamed ? reassemble(body.text, format) : null
  const modes: Mode[] = assembled !== null ? ['text', 'raw'] : ['pretty', 'raw']
  const active = mode && modes.includes(mode) ? mode : modes[0]
  const shown = active === 'text' ? assembled ?? '' : active === 'pretty' ? prettyJson(body.text) : body.text
  return (
    <div>
      <div className="mb-2 flex flex-wrap items-center gap-2 text-xs text-zinc-500">
        <span>
          {formatBytes(body.original_bytes)} original
          {body.truncated ? ' · truncated' : ''}
          {body.redacted ? ' · redacted' : ''} · {body.content_type.split(';')[0]}
        </span>
        <span className="ml-auto flex gap-1">
          {modes.map((m) => (
            <button key={m} type="button" onClick={() => setMode(m)} aria-pressed={active === m} className={`rounded px-2 py-0.5 ${active === m ? 'bg-zinc-700 text-zinc-50' : 'text-zinc-400 hover:bg-zinc-800'}`}>
              {m === 'text' ? 'reply text' : m}
            </button>
          ))}
        </span>
      </div>
      <pre className="max-h-[32rem] overflow-auto rounded bg-zinc-950 p-3 font-mono text-xs leading-relaxed whitespace-pre-wrap break-words text-zinc-200">{shown}</pre>
    </div>
  )
}
