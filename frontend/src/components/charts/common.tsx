import type { ReactNode } from 'react'
import { AXIS, GRID, MUTED, SURFACE, TEXT_SECONDARY } from '../../lib/colours'
import { formatClock, formatCompact } from '../../lib/format'

export const CHART_HEIGHT = 200

export const axisTick = { fill: MUTED, fontSize: 11 }
export const axisLine = { stroke: AXIS }
export const gridProps = { stroke: GRID, vertical: false, strokeDasharray: undefined }
export const cursorProps = { stroke: AXIS, strokeWidth: 1 }
export const activeDot = { r: 4, strokeWidth: 2, stroke: SURFACE }

export function timeTick(t: number): string {
  return formatClock(new Date(t)).slice(0, 5)
}

export function compactTick(v: number): string {
  return formatCompact(v)
}

/** Legend text wears the text token, never the series colour. */
export function legendFormatter(value: string): ReactNode {
  return <span style={{ color: TEXT_SECONDARY, fontSize: 11 }}>{value}</span>
}

interface TooltipRow {
  name: string
  value: string
  color: string
}

/** Values lead, labels follow; series keyed by a short stroke of their colour. */
export function TooltipBox({ title, rows }: { title: string; rows: TooltipRow[] }) {
  return (
    <div className="rounded border border-zinc-700 bg-zinc-900/95 px-2 py-1.5 font-mono text-xs shadow">
      <div className="mb-1 text-zinc-400">{title}</div>
      {rows.map((r) => (
        <div key={r.name} className="flex items-center gap-2">
          <span className="inline-block h-0.5 w-3" style={{ background: r.color }} />
          <span className="tabular-nums text-zinc-50">{r.value}</span>
          <span className="text-zinc-400">{r.name}</span>
        </div>
      ))}
    </div>
  )
}

export interface RechartsTooltipProps {
  active?: boolean
  label?: number | string
  payload?: { name?: string | number; value?: number | string; color?: string; dataKey?: string | number }[]
}

export function makeTooltip(format: (v: number) => string) {
  return function ChartTooltip({ active, label, payload }: RechartsTooltipProps) {
    if (!active || !payload || payload.length === 0) return null
    const rows = payload
      .filter((p) => typeof p.value === 'number')
      .map((p) => ({
        name: String(p.name ?? p.dataKey ?? ''),
        value: format(p.value as number),
        color: p.color ?? MUTED,
      }))
    const title = typeof label === 'number' ? formatClock(new Date(label)) : String(label ?? '')
    return <TooltipBox title={title} rows={rows} />
  }
}
