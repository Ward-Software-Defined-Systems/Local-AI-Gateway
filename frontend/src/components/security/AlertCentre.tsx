import { useMemo, useState } from 'react'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import { Link } from 'react-router'
import { actions, useAlerts } from '../../api/hooks'
import type { Alert } from '../../api/types'
import { severityClass } from '../../lib/colours'
import { formatDateTime } from '../../lib/format'

const button = 'rounded border border-zinc-700 px-2 py-0.5 text-xs text-zinc-300 hover:bg-zinc-800 disabled:opacity-50'

function suppressed(alert: Alert): number {
  const n = alert.detail['suppressed_before_this']
  return typeof n === 'number' ? n : 0
}

export default function AlertCentre() {
  const [unackedOnly, setUnackedOnly] = useState(true)
  const alerts = useAlerts(unackedOnly)
  const rows = useMemo(() => alerts.data?.pages.flatMap((p) => p.items) ?? [], [alerts.data])
  const unacked = alerts.data?.pages[0]?.unacked ?? 0
  const qc = useQueryClient()
  const invalidate = () => {
    void qc.invalidateQueries({ queryKey: ['alerts'] })
    void qc.invalidateQueries({ queryKey: ['status'] })
  }
  const ack = useMutation({ mutationFn: actions.ackAlert, onSuccess: invalidate })
  const ackAll = useMutation({ mutationFn: actions.ackAllAlerts, onSuccess: invalidate })

  return (
    <div>
      <div className="mb-2 flex flex-wrap items-center gap-3 text-xs">
        <label className="flex items-center gap-1 text-zinc-400">
          <input type="checkbox" checked={unackedOnly} onChange={(e) => setUnackedOnly(e.target.checked)} /> unacknowledged only
        </label>
        <span className="text-zinc-500">{unacked} unacknowledged</span>
        <button type="button" className={button} onClick={() => ackAll.mutate()} disabled={ackAll.isPending || unacked === 0}>
          acknowledge all
        </button>
        {(ack.isError || ackAll.isError) && <span className="text-red-300">{((ack.error ?? ackAll.error) as Error).message}</span>}
      </div>
      {alerts.isError && <p className="text-sm text-red-300">{(alerts.error as Error).message}</p>}
      {rows.length === 0 ? (
        <p className="text-sm text-zinc-500">{unackedOnly ? 'No unacknowledged alerts.' : 'No alerts yet.'}</p>
      ) : (
        <div className="max-h-80 overflow-auto">
          <table className="w-full font-mono text-xs">
            <thead className="sticky top-0 bg-zinc-900 text-left text-zinc-500">
              <tr>
                <th className="pb-1 pr-3 font-normal">time</th>
                <th className="pb-1 pr-3 font-normal">severity</th>
                <th className="pb-1 pr-3 font-normal">kind</th>
                <th className="pb-1 pr-3 font-normal">alert</th>
                <th className="pb-1 pr-3 font-normal">delivered</th>
                <th className="pb-1 pr-3 font-normal">request</th>
                <th className="pb-1 font-normal" />
              </tr>
            </thead>
            <tbody>
              {rows.map((a) => (
                <tr key={a.id} className={`border-t border-zinc-800/60 ${a.acknowledged_at ? 'text-zinc-500' : ''}`}>
                  <td className="py-1 pr-3 whitespace-nowrap tabular-nums">{formatDateTime(a.created_at)}</td>
                  <td className={`py-1 pr-3 ${severityClass(a.severity)}`}>{a.severity}</td>
                  <td className="py-1 pr-3 whitespace-nowrap text-zinc-400">{a.kind.replace(/_/g, ' ')}</td>
                  <td className="py-1 pr-3">
                    {a.title}
                    {suppressed(a) > 0 && <span className="ml-2 text-zinc-500">+{suppressed(a)} suppressed</span>}
                  </td>
                  <td className="py-1 pr-3 text-zinc-500">{a.delivered.join(', ')}</td>
                  <td className="py-1 pr-3">
                    {a.request_id ? (
                      <Link to={`/requests/${a.request_id}`} className="text-zinc-300 hover:underline">
                        open
                      </Link>
                    ) : (
                      <span className="text-zinc-600">-</span>
                    )}
                  </td>
                  <td className="py-1 text-right">
                    {a.acknowledged_at ? (
                      <span className="text-zinc-600" title={formatDateTime(a.acknowledged_at)}>
                        acked
                      </span>
                    ) : (
                      <button type="button" className={button} onClick={() => ack.mutate(a.id)} disabled={ack.isPending}>
                        ack
                      </button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {alerts.hasNextPage && (
        <button type="button" className={`${button} mt-2`} onClick={() => void alerts.fetchNextPage()} disabled={alerts.isFetchingNextPage}>
          {alerts.isFetchingNextPage ? 'loading…' : 'load more'}
        </button>
      )}
    </div>
  )
}
