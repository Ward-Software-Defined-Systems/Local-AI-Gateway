import type { ApiFormat } from '../api/types'

/** The assistant text reassembled from a captured streaming body, or `null` when the body is not a stream. */
export function reassemble(body: string, format: ApiFormat): string | null {
  if (format === 'ollama_native') return reassembleNdjson(body)
  const frames = sseFrames(body)
  if (frames.length === 0) return null
  const parts: string[] = []
  for (const frame of frames) {
    if (frame.data === '[DONE]') continue
    let json: unknown
    try {
      json = JSON.parse(frame.data)
    } catch {
      continue
    }
    const text = deltaText(json, format)
    if (text) parts.push(text)
  }
  return parts.length ? parts.join('') : null
}

interface Frame {
  event: string | null
  data: string
}

export function sseFrames(body: string): Frame[] {
  const frames: Frame[] = []
  let event: string | null = null
  let data: string[] = []
  const flush = () => {
    if (data.length || event) frames.push({ event, data: data.join('\n') })
    event = null
    data = []
  }
  for (const rawLine of body.split(/\r?\n/)) {
    const line = rawLine
    if (line === '') {
      flush()
      continue
    }
    if (line.startsWith(':')) continue
    const idx = line.indexOf(':')
    const field = idx === -1 ? line : line.slice(0, idx)
    let value = idx === -1 ? '' : line.slice(idx + 1)
    if (value.startsWith(' ')) value = value.slice(1)
    if (field === 'data') data.push(value)
    else if (field === 'event') event = value
  }
  flush()
  return frames
}

function reassembleNdjson(body: string): string | null {
  const parts: string[] = []
  let any = false
  for (const line of body.split('\n')) {
    if (!line.trim()) continue
    try {
      const obj = JSON.parse(line) as { message?: { content?: string }; response?: string }
      any = true
      if (typeof obj.message?.content === 'string') parts.push(obj.message.content)
      else if (typeof obj.response === 'string') parts.push(obj.response)
    } catch {
      // a non-JSON line is not a chunk
    }
  }
  return any ? parts.join('') : null
}

function deltaText(json: unknown, format: ApiFormat): string | null {
  if (typeof json !== 'object' || json === null) return null
  const obj = json as Record<string, unknown>
  switch (format) {
    case 'anthropic_messages': {
      const delta = obj.delta as { type?: string; text?: string } | undefined
      return delta?.type === 'text_delta' && typeof delta.text === 'string' ? delta.text : null
    }
    case 'openai_responses':
      return obj.type === 'response.output_text.delta' && typeof obj.delta === 'string' ? obj.delta : null
    case 'gemini_generate': {
      const candidates = obj.candidates as { content?: { parts?: { text?: unknown }[] } }[] | undefined
      if (!Array.isArray(candidates)) return null
      const parts = candidates
        .flatMap((c) => c.content?.parts ?? [])
        .map((p) => p.text)
        .filter((t): t is string => typeof t === 'string')
      return parts.length ? parts.join('') : null
    }
    case 'gemini_interactions': {
      if (obj.event_type !== 'step.delta') return null
      const delta = obj.delta as { type?: string; text?: string; content?: { text?: unknown } } | undefined
      if (delta?.type === 'text' && typeof delta.text === 'string') return delta.text
      if (delta?.type === 'thought_summary' && typeof delta.content?.text === 'string') return delta.content.text
      return null
    }
    default: {
      const choices = obj.choices as { delta?: { content?: string }; text?: string; message?: { content?: string } }[] | undefined
      if (!Array.isArray(choices)) return null
      const parts = choices.map((c) => c.delta?.content ?? c.text ?? c.message?.content ?? '').filter((s) => typeof s === 'string')
      return parts.length ? parts.join('') : null
    }
  }
}

/** Pretty JSON when the text parses, otherwise the text itself. */
export function prettyJson(text: string): string {
  try {
    return JSON.stringify(JSON.parse(text), null, 2)
  } catch {
    return text
  }
}
