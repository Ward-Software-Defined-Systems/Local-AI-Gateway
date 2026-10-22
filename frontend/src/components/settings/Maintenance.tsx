import { useMutation, useQueryClient } from '@tanstack/react-query'
import { actions } from '../../api/hooks'
import { useLive } from '../../store/live'

const button = 'rounded border border-zinc-700 px-2 py-1 text-xs text-zinc-300 hover:bg-zinc-800 disabled:opacity-50'

/** Housekeeping that the TUIs did with a key press: run retention, reset the session metrics, clear alerts. */
export default function Maintenance() {
  const qc = useQueryClient()
  const resetFeed = useLive((s) => s.resetFeed)
  const retention = useMutation({
    mutationFn: actions.runRetention,
    onSuccess: () => void qc.invalidateQueries({ queryKey: ['status'] }),
  })
  const reset = useMutation({
    mutationFn: actions.resetSession,
    onSuccess: () => {
      resetFeed()
      void qc.invalidateQueries({ queryKey: ['rolling'] })
    },
  })
  const ackAll = useMutation({
    mutationFn: actions.ackAllAlerts,
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ['alerts'] })
      void qc.invalidateQueries({ queryKey: ['status'] })
    },
  })
  const r = retention.data
  return (
    <div className="space-y-2 text-xs">
      <div className="flex flex-wrap items-center gap-2">
        <button type="button" className={button} onClick={() => retention.mutate()} disabled={retention.isPending}>
          run retention now
        </button>
        <button type="button" className={button} onClick={() => reset.mutate()} disabled={reset.isPending} title="restart the rolling windows and clear the live feed">
          reset session metrics
        </button>
        <button type="button" className={button} onClick={() => ackAll.mutate()} disabled={ackAll.isPending}>
          acknowledge all alerts
        </button>
      </div>
      {r && (
        <p className="font-mono text-zinc-400">
          retention: {r.bodies_deleted} bodies, {r.passthrough_records_deleted} passthrough records, {r.hardware_samples_deleted} hardware samples,{' '}
          {r.alerts_deleted} acknowledged alerts removed
        </p>
      )}
      {reset.isSuccess && <p className="text-zinc-400">Session metrics reset.</p>}
      {ackAll.isSuccess && <p className="text-zinc-400">{ackAll.data.acknowledged} alert(s) acknowledged.</p>}
      {(retention.error ?? reset.error ?? ackAll.error) && (
        <p className="text-red-300">{((retention.error ?? reset.error ?? ackAll.error) as Error).message}</p>
      )}
      <p className="text-zinc-500">
        Retention also runs every ten minutes: bodies past <code>capture.retention_days</code>, passthrough records past{' '}
        <code>capture.passthrough_retention_hours</code>, raw hardware samples past <code>capture.hardware_raw_retention_hours</code>, acknowledged
        alerts past 90 days. Request metadata and findings are kept.
      </p>
    </div>
  )
}
