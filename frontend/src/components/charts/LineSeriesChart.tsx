import { CartesianGrid, Legend, Line, LineChart, ResponsiveContainer, Tooltip, XAxis, YAxis } from 'recharts'
import type { Series } from '../../api/types'
import { SERIES } from '../../lib/colours'
import { formatClock } from '../../lib/format'
import ChartCard from './ChartCard'
import { CHART_HEIGHT, activeDot, axisLine, axisTick, compactTick, cursorProps, gridProps, legendFormatter, makeTooltip, timeTick } from './common'

export interface LineSpec {
  key: string
  label: string
  /** A hex colour; categorical slots by default, status tokens when the series means good or bad. */
  color?: string
}

interface Props {
  title: string
  series: Series | undefined
  lines: LineSpec[]
  format: (v: number) => string
  loading?: boolean
}

/** A time series of one or more lines on one axis: 2px strokes, hairline grid, crosshair tooltip, legend for two or more. */
export default function LineSeriesChart({ title, series, lines, format, loading }: Props) {
  const data: Record<string, number>[] = (series?.points ?? []).map((p) => ({ ...p.values, t: new Date(p.t).getTime() }))
  const table = {
    columns: ['time', ...lines.map((l) => l.label)],
    rows: data.map((d) => [formatClock(new Date(d.t)), ...lines.map((l) => (typeof d[l.key] === 'number' ? format(d[l.key]) : '-'))]),
  }
  const Tip = makeTooltip(format)
  return (
    <ChartCard title={title} table={table} loading={loading} empty={data.length === 0}>
      <ResponsiveContainer width="100%" height={CHART_HEIGHT}>
        <LineChart data={data} margin={{ top: 8, right: 12, left: 0, bottom: 0 }}>
          <CartesianGrid {...gridProps} />
          <XAxis dataKey="t" type="number" domain={['dataMin', 'dataMax']} scale="time" tickFormatter={timeTick} tick={axisTick} axisLine={axisLine} tickLine={false} minTickGap={40} />
          <YAxis tickFormatter={compactTick} tick={axisTick} axisLine={false} tickLine={false} width={44} />
          <Tooltip content={<Tip />} cursor={cursorProps} />
          {lines.length > 1 && <Legend formatter={legendFormatter} iconType="plainline" />}
          {lines.map((l, i) => (
            <Line key={l.key} type="monotone" dataKey={l.key} name={l.label} stroke={l.color ?? SERIES[i]} strokeWidth={2} dot={false} activeDot={activeDot} isAnimationActive={false} connectNulls />
          ))}
        </LineChart>
      </ResponsiveContainer>
    </ChartCard>
  )
}
