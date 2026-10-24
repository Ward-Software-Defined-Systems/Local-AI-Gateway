import { useState } from 'react'
import { useMutation } from '@tanstack/react-query'
import { actions } from '../../api/hooks'
import { BACKEND_NAMES, CLOUD_BACKENDS, type Backend } from '../../api/types'
import FindingsTable from '../requests/FindingsTable'

const select = 'rounded border border-zinc-700 bg-zinc-900 px-2 py-1 text-xs text-zinc-200'
const button = 'rounded border border-zinc-700 px-2 py-1 text-xs text-zinc-300 hover:bg-zinc-800 disabled:opacity-50'

const EXAMPLE = `{"model":"example","messages":[{"role":"user","content":"deploy with AKIAIOSFODNN7EXAMPLE and mail ops@example.org"}]}`

/** Scans pasted text under the live policy. Nothing typed here is stored, logged or forwarded. */
export default function TestBox() {
  const [text, setText] = useState('')
  const [backend, setBackend] = useState<Backend>('ollama')
  const [cloud, setCloud] = useState(false)
  // A request to a cloud provider always leaves the machine, whatever the box says.
  const toProvider = CLOUD_BACKENDS.includes(backend)
  const leavesMachine = cloud || toProvider
  const scan = useMutation({ mutationFn: () => actions.dlpTest(text, backend, leavesMachine) })
  const result = scan.data

  return (
    <div className="space-y-2">
      <textarea
        className="h-28 w-full rounded border border-zinc-700 bg-zinc-950 p-2 font-mono text-xs text-zinc-200 placeholder:text-zinc-600"
        placeholder="paste a request body or any text; JSON is walked leaf by leaf"
        value={text}
        onChange={(e) => setText(e.target.value)}
        aria-label="text to scan"
      />
      <div className="flex flex-wrap items-center gap-2 text-xs">
        <select className={select} value={backend} onChange={(e) => setBackend(e.target.value as Backend)} aria-label="backend">
          {(Object.keys(BACKEND_NAMES) as Backend[]).map((b) => (
            <option key={b} value={b}>
              as a request to {BACKEND_NAMES[b]}
            </option>
          ))}
        </select>
        <label className="flex items-center gap-1 text-zinc-400" title={toProvider ? 'every request to a cloud provider leaves the machine' : 'an Ollama :cloud model'}>
          <input type="checkbox" checked={leavesMachine} disabled={toProvider} onChange={(e) => setCloud(e.target.checked)} /> leaves the machine
        </label>
        <button type="button" className={button} onClick={() => scan.mutate()} disabled={scan.isPending || text.trim() === ''}>
          {scan.isPending ? 'scanning…' : 'scan'}
        </button>
        <button type="button" className={button} onClick={() => setText(EXAMPLE)}>
          insert example
        </button>
        <span className="text-zinc-500">nothing entered here is stored or logged</span>
        {scan.isError && <span className="text-red-300">{(scan.error as Error).message}</span>}
      </div>
      {result && (
        <div className="space-y-2">
          {!result.enabled && <p className="text-sm ink-critical">DLP is disabled (dlp.enabled = false); the proxy scans nothing.</p>}
          {result.would_block ? (
            <p className="text-sm ink-critical">This request would be blocked (403) by rule {result.would_block}; the backend would never see it.</p>
          ) : (
            <p className="text-sm text-zinc-400">
              {result.findings.length === 0 ? 'No findings.' : `${result.findings.length} finding(s); the request would be forwarded.`}
              {result.allowlisted > 0 && ` ${result.allowlisted} match(es) removed by the allowlist.`}
              {result.scan_truncated && ' The text is longer than dlp.max_scan_bytes; only the start was scanned.'}
            </p>
          )}
          {result.findings.length > 0 && <FindingsTable findings={result.findings} />}
          {result.redacted !== null && (
            <div>
              <div className="mb-1 text-xs text-zinc-500">stored copy (redacted)</div>
              <pre className="max-h-40 overflow-auto rounded border border-zinc-800 bg-zinc-950 p-2 font-mono text-xs whitespace-pre-wrap break-all text-zinc-300">{result.redacted}</pre>
            </div>
          )}
        </div>
      )}
    </div>
  )
}
