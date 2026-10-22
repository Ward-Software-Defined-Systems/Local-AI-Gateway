import { useEffect, useState } from 'react'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import { actions, useConfigFile } from '../../api/hooks'
import type { ApplyReport, ConfigCheck } from '../../api/types'

const button = 'rounded border border-zinc-700 px-2 py-1 text-xs text-zinc-300 hover:bg-zinc-800 disabled:opacity-50'

function Report({ report }: { report: ApplyReport }) {
  return (
    <div className="space-y-1 text-xs">
      <p className="text-zinc-300">
        Applied: {report.applied.length ? report.applied.join(', ') : 'no changes'} · {report.rules_enabled} rules enabled · {report.known_secrets} known
        secret(s)
      </p>
      {report.restart_required.length > 0 && (
        <p className="rounded bg-amber-500/15 px-2 py-1 text-amber-300">
          Restart the gateway for: {report.restart_required.join(', ')}. Listeners, the dashboard address, hardware sampling and logging
          only change at start.
        </p>
      )}
      {report.warnings.map((w) => (
        <p key={w} className="text-amber-300/80">
          warning: {w}
        </p>
      ))}
    </div>
  )
}

/** config.toml as text: check, save (what can be applied is applied at once), reload from disk. */
export default function ConfigEditor() {
  const file = useConfigFile()
  const qc = useQueryClient()
  const [text, setText] = useState<string | null>(null)
  const [check, setCheck] = useState<ConfigCheck | null>(null)
  const [reloaded, setReloaded] = useState<ApplyReport | null>(null)
  useEffect(() => {
    if (file.data && text === null) setText(file.data.text)
  }, [file.data, text])

  const afterChange = () => {
    void qc.invalidateQueries({ queryKey: ['config-file'] })
    void qc.invalidateQueries({ queryKey: ['dlp-rules'] })
    void qc.invalidateQueries({ queryKey: ['status'] })
  }
  const validate = useMutation({
    mutationFn: (t: string) => actions.validateConfig(t),
    onSuccess: (c) => {
      setCheck(c)
      setReloaded(null)
    },
  })
  const save = useMutation({
    mutationFn: (t: string) => actions.saveConfig(t),
    onSuccess: (c) => {
      setCheck(c)
      setReloaded(null)
      if (c.saved) afterChange()
    },
  })
  const reload = useMutation({
    mutationFn: actions.reloadConfig,
    onSuccess: (r) => {
      setReloaded(r)
      setCheck(null)
      setText(null)
      afterChange()
    },
  })

  if (file.isError) return <p className="text-sm text-red-300">{(file.error as Error).message}</p>
  if (!file.data || text === null) return <p className="text-sm text-zinc-500">Loading…</p>
  const dirty = text !== file.data.text
  const busy = validate.isPending || save.isPending || reload.isPending
  const error = (validate.error ?? save.error ?? reload.error) as Error | null

  return (
    <div className="space-y-2">
      <div className="flex flex-wrap items-center gap-2 text-xs">
        <span className="font-mono text-zinc-500">{file.data.path}</span>
        {!file.data.exists && <span className="text-zinc-500">(no file yet: the gateway runs on defaults)</span>}
        {dirty && <span className="text-amber-300">unsaved changes</span>}
      </div>
      <textarea
        className="h-96 w-full rounded border border-zinc-700 bg-zinc-950 p-2 font-mono text-xs leading-5 text-zinc-200"
        value={text}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          if ((e.metaKey || e.ctrlKey) && e.key === 's') {
            e.preventDefault()
            if (!busy) save.mutate(text)
          }
        }}
        spellCheck={false}
        aria-label="config.toml"
      />
      <div className="flex flex-wrap items-center gap-2 text-xs">
        <button type="button" className={button} onClick={() => validate.mutate(text)} disabled={busy}>
          check
        </button>
        <button type="button" className={`${button} border-cyan-700 text-cyan-200`} onClick={() => save.mutate(text)} disabled={busy}>
          save and apply
        </button>
        <button type="button" className={button} onClick={() => setText(file.data?.text ?? '')} disabled={busy || !dirty}>
          revert
        </button>
        <button type="button" className={button} onClick={() => reload.mutate()} disabled={busy} title="re-read the file from disk and apply it (as SIGHUP does)">
          reload from disk
        </button>
        {text.trim() === '' && (
          <button type="button" className={button} onClick={() => setText(file.data?.sample ?? '')} disabled={busy}>
            start from the commented sample
          </button>
        )}
        <span className="text-zinc-500">⌘S / Ctrl+S saves</span>
        {error && <span className="text-red-300">{error.message}</span>}
      </div>
      {check && (
        <div className="space-y-1">
          {check.issues.length === 0 && <p className="text-xs ink-good">{check.saved ? 'Saved and applied.' : 'No issues.'}</p>}
          {check.issues.length > 0 && check.saved && <p className="text-xs ink-good">Saved and applied, with warnings:</p>}
          {check.issues.length > 0 && !check.saved && !check.ok && <p className="text-xs ink-critical">Not saved: fix the errors below.</p>}
          {check.issues.length > 0 && !check.saved && check.ok && <p className="text-xs text-zinc-300">Valid, with warnings:</p>}
          <ul className="space-y-0.5 font-mono text-xs">
            {check.issues.map((i, n) => (
              <li key={n} className={i.level === 'error' ? 'ink-critical' : 'text-amber-300/80'}>
                {i.level}: {i.message}
              </li>
            ))}
          </ul>
          {check.report && <Report report={check.report} />}
        </div>
      )}
      {reloaded && <Report report={reloaded} />}
      <p className="text-xs text-zinc-500">
        Sections applied live: <code>[capture]</code>, <code>[dlp]</code>, <code>[alerts]</code>, <code>[pricing]</code>. The service also reloads on
        SIGHUP. Webhook header values are shown as written in the file.
      </p>
    </div>
  )
}
