import { useState } from 'react'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import { actions, useDlpRules } from '../../api/hooks'
import type { ApplyReport, DlpAction, RuleView } from '../../api/types'
import { severityClass } from '../../lib/colours'

const button = 'rounded border border-zinc-700 px-2 py-0.5 text-xs text-zinc-300 hover:bg-zinc-800 disabled:opacity-50'

function actionClass(a: DlpAction): string {
  return a === 'block' ? 'ink-critical' : a === 'alert' ? 'ink-warning' : 'text-zinc-400'
}

function hint(rule: RuleView): string {
  const parts = [rule.description]
  if (rule.keywords.length) parts.push(`keywords: ${rule.keywords.join(', ')}`)
  if (rule.min_entropy !== null) parts.push(`entropy ≥ ${rule.min_entropy} bits/char`)
  if (rule.validator) parts.push(`validator: ${rule.validator}`)
  return parts.join(' · ')
}

/** Every rule with the action it resolves to per backend; the policy itself is edited in config.toml. */
export default function RulesMatrix() {
  const rules = useDlpRules()
  const qc = useQueryClient()
  const [report, setReport] = useState<ApplyReport | null>(null)
  const reload = useMutation({
    mutationFn: actions.reloadConfig,
    onSuccess: (r) => {
      setReport(r)
      void qc.invalidateQueries({ queryKey: ['dlp-rules'] })
      void qc.invalidateQueries({ queryKey: ['status'] })
    },
  })
  const data = rules.data
  if (rules.isError) return <p className="text-sm text-red-300">{(rules.error as Error).message}</p>
  if (!data) return <p className="text-sm text-zinc-500">Loading…</p>

  return (
    <div>
      <div className="mb-2 flex flex-wrap items-center gap-x-4 gap-y-1 font-mono text-xs text-zinc-400">
        <span>
          dlp <span className={data.enabled ? 'ink-good' : 'ink-critical'}>{data.enabled ? 'on' : 'off'}</span>
        </span>
        <span>
          default <span className={actionClass(data.default_action)}>{data.default_action}</span>
        </span>
        <span>
          replies <span className={actionClass(data.response_action)}>{data.response_action}</span>
        </span>
        <span>
          cloud secrets <span className={actionClass(data.cloud.request_action)}>{data.cloud.request_action}</span> · cloud PII{' '}
          <span className={actionClass(data.cloud.pii_action)}>{data.cloud.pii_action}</span>
        </span>
        <span>notify ≥ {data.notify_min_severity}</span>
        <span>{data.redact_stored_copies ? 'stored copies redacted' : 'stored copies NOT redacted'}</span>
        <span title={data.known_secrets_file}>
          {data.known_secrets} known secret(s) · {data.allowlist_entries} allowlist · {data.custom_rules} custom
        </span>
        <button type="button" className={button} onClick={() => reload.mutate()} disabled={reload.isPending} title="re-read config.toml and the known-secrets file">
          {reload.isPending ? 'reloading…' : 'reload from disk'}
        </button>
        {reload.isError && <span className="text-red-300">{(reload.error as Error).message}</span>}
        {report && (
          <span className="text-zinc-500">
            reloaded: {report.applied.length ? report.applied.join(', ') : 'no changes'}
            {report.restart_required.length ? ` · restart required for ${report.restart_required.join(', ')}` : ''}
          </span>
        )}
      </div>
      <div className="max-h-[32rem] overflow-auto">
        <table className="w-full font-mono text-xs">
          <thead className="sticky top-0 bg-zinc-900 text-left text-zinc-500">
            <tr>
              <th className="pb-1 pr-3 font-normal">rule</th>
              <th className="pb-1 pr-3 font-normal">category</th>
              <th className="pb-1 pr-3 font-normal">severity</th>
              <th className="pb-1 pr-3 font-normal">enabled</th>
              <th className="pb-1 pr-3 font-normal">redact</th>
              <th className="pb-1 pr-3 font-normal">LM Studio</th>
              <th className="pb-1 pr-3 font-normal">Ollama</th>
              <th className="pb-1 pr-3 font-normal">Ollama cloud</th>
              <th className="pb-1 font-normal">replies</th>
            </tr>
          </thead>
          <tbody>
            {data.rules.map((r) => (
              <tr key={r.id} className={`border-t border-zinc-800/60 ${r.enabled ? '' : 'text-zinc-600'}`} title={hint(r)}>
                <td className="py-1 pr-3 whitespace-nowrap">
                  {r.id}
                  {!r.builtin && <span className="ml-1 text-zinc-500">custom</span>}
                </td>
                <td className="py-1 pr-3 text-zinc-400">{r.category.replace('_', ' ')}</td>
                <td className={`py-1 pr-3 ${r.enabled ? severityClass(r.severity) : ''}`}>{r.severity}</td>
                <td className="py-1 pr-3">{r.enabled ? 'yes' : 'no'}</td>
                <td className="py-1 pr-3 text-zinc-400">{r.redact ? 'yes' : 'no'}</td>
                <td className={`py-1 pr-3 ${r.enabled ? actionClass(r.request_action.lmstudio) : ''}`}>{r.request_action.lmstudio}</td>
                <td className={`py-1 pr-3 ${r.enabled ? actionClass(r.request_action.ollama) : ''}`}>{r.request_action.ollama}</td>
                <td className={`py-1 pr-3 ${r.enabled ? actionClass(r.request_action.cloud) : ''}`}>{r.request_action.cloud}</td>
                <td className="py-1 text-zinc-400">
                  {r.response_action.lmstudio === r.response_action.ollama
                    ? r.response_action.ollama
                    : `${r.response_action.lmstudio} / ${r.response_action.ollama}`}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <p className="mt-2 text-xs text-zinc-500">
        Change a rule under <code>[dlp.rules.&lt;id&gt;]</code>, add your own under <code>[[dlp.custom_rules]]</code> and exempt values under{' '}
        <code>[[dlp.allowlist]]</code> in config.toml (Settings tab); the engine reloads without a restart.
      </p>
    </div>
  )
}
