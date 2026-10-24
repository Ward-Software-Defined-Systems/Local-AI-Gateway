import type { RequestRecord } from '../api/types'

/** A `curl` command that replays the request against the listener it came through, credentials redacted. */
export function copyAsCurl(record: RequestRecord, body: string | null): string {
  const [host, port] = record.listener.split(':')
  const shownHost = host === '0.0.0.0' || host === '[::]' || host === '::' ? '127.0.0.1' : host
  const url = `http://${shownHost}:${port}${record.path}${record.query ? `?${record.query}` : ''}`
  const parts = ['curl', '-sN', record.method !== 'GET' ? `-X ${record.method}` : '', shellQuote(url)].filter(Boolean)
  for (const [name, value] of record.request_headers) {
    if (['host', 'content-length', 'connection', 'accept-encoding', 'x-request-id'].includes(name)) continue
    const shown = name === 'authorization' ? 'Bearer <REDACTED>' : name === 'x-api-key' || name === 'x-goog-api-key' || name === 'cookie' ? '<REDACTED>' : value
    parts.push('-H', shellQuote(`${name}: ${shown}`))
  }
  if (body !== null && body !== '' && record.method !== 'GET') {
    parts.push('--data-binary', shellQuote(body))
  }
  return parts.join(' \\\n  ')
}

function shellQuote(s: string): string {
  return `'${s.replace(/'/g, `'\\''`)}'`
}
