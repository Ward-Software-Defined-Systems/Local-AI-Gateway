import { useMemo, useRef } from 'react'
import { Bar, BarChart, CartesianGrid, Legend, ResponsiveContainer, Tooltip, XAxis, YAxis } from 'recharts'
import type { Series } from '../../api/types'
import { SERIES_OTHER, SURFACE, SlotAssigner } from '../../lib/colours'
import { formatClock, formatCompact } from '../../lib/format'
import ChartCard from './ChartCard'
import { CHART_HEIGHT, axisLine, axisTick, compactTick, gridProps, legendFormatter, makeTooltip, timeTick } from './common'

/** Series beyond this fold into "Other" rather than taking a ninth hue. */
const MAX_SERIES = 7

interface Props {
  title: string
  series: Series | undefined
  loading?: boolean
}

/** Tokens per model (or client) per bucket as thin stacked columns with a 2px surface gap between segments. */
export default function StackedTokensChart({ title, series, loading }: Props) {
  const slots = useRef(new SlotAssigner())
  const { data, keys } = useMemo(() => {
    const points = series?.points ?? []
    const totals = new Map<string, number>()
    for (const p of points) for (const [k, v] of Object.entries(p.values)) totals.set(k, (totals.get(k) ?? 0) + v)
    const ranked = [...totals.entries()].sort((a, b) => b[1] - a[1]).map(([k]) => k)
    const kept = ranked.slice(0, MAX_SERIES)
    const folded = new Set(ranked.slice(MAX_SERIES))
    const data = points.map((p) => {
      const row: Record<string, number> = { t: new Date(p.t).getTime() }
      for (const [k, v] of Object.entries(p.values)) {
        const key = folded.has(k) ? 'Other' : k
        row[key] = (row[key] ?? 0) + v
      }
      return row
    })
    return { data, keys: folded.size > 0 ? [...kept, 'Other'] : kept }
  }, [series])
  const table = {
    columns: ['time', ...keys],
    rows: data.map((d) => [formatClock(new Date(d.t)), ...keys.map((k) => (typeof d[k] === 'number' ? formatCompact(d[k]) : '-'))]),
  }
  const Tip = makeTooltip(formatCompact)
  return (
    <ChartCard title={title} table={table} loading={loading} empty={data.length === 0}>
      <ResponsiveContainer width="100%" height={CHART_HEIGHT}>
        <BarChart data={data} margin={{ top: 8, right: 12, left: 0, bottom: 0 }} barCategoryGap="30%" maxBarSize={24}>
          <CartesianGrid {...gridProps} />
          <XAxis dataKey="t" type="number" domain={['dataMin', 'dataMax']} scale="time" tickFormatter={timeTick} tick={axisTick} axisLine={axisLine} tickLine={false} minTickGap={40} />
          <YAxis tickFormatter={compactTick} tick={axisTick} axisLine={false} tickLine={false} width={44} />
          <Tooltip content={<Tip />} cursor={{ fill: 'rgba(255,255,255,0.04)' }} />
          {keys.length > 1 && <Legend formatter={legendFormatter} iconType="square" />}
          {keys.map((k, i) => (
            <Bar
              key={k}
              dataKey={k}
              name={k}
              stackId="tokens"
              fill={k === 'Other' ? SERIES_OTHER : slots.current.slot(k)}
              stroke={SURFACE}
              strokeWidth={2}
              radius={i === keys.length - 1 ? [4, 4, 0, 0] : 0}
              isAnimationActive={false}
            />
          ))}
        </BarChart>
      </ResponsiveContainer>
    </ChartCard>
  )
}
