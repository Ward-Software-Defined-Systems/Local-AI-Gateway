import { Area, AreaChart, CartesianGrid, ResponsiveContainer, Tooltip, XAxis, YAxis } from 'recharts'
import type { HardwarePoint } from '../../api/types'
import { SERIES } from '../../lib/colours'
import { formatBytes, formatClock, formatPct } from '../../lib/format'
import ChartCard from './ChartCard'
import { activeDot, axisLine, axisTick, cursorProps, gridProps, makeTooltip, timeTick } from './common'

interface Props {
  points: HardwarePoint[] | undefined
  loading?: boolean
  /** Linux shows VRAM; macOS unified GPU memory. */
  gpuMemLabel: string
}

function Single({ title, data, dataKey, format, domain }: { title: string; data: Record<string, number | null>[]; dataKey: string; format: (v: number) => string; domain?: [number, number | 'auto'] }) {
  const Tip = makeTooltip(format)
  const table = { columns: ['time', title], rows: data.map((d) => [formatClock(new Date(d.t as number)), typeof d[dataKey] === 'number' ? format(d[dataKey] as number) : '-']) }
  return (
    <ChartCard title={title} table={table} empty={data.length === 0}>
      <ResponsiveContainer width="100%" height={140}>
        <AreaChart data={data} margin={{ top: 8, right: 12, left: 0, bottom: 0 }}>
          <CartesianGrid {...gridProps} />
          <XAxis dataKey="t" type="number" domain={['dataMin', 'dataMax']} scale="time" tickFormatter={timeTick} tick={axisTick} axisLine={axisLine} tickLine={false} minTickGap={40} />
          <YAxis tickFormatter={(v: number) => format(v)} tick={axisTick} axisLine={false} tickLine={false} width={52} domain={domain} />
          <Tooltip content={<Tip />} cursor={cursorProps} />
          <Area type="monotone" dataKey={dataKey} name={title} stroke={SERIES[0]} strokeWidth={2} fill={SERIES[0]} fillOpacity={0.1} dot={false} activeDot={activeDot} isAnimationActive={false} connectNulls />
        </AreaChart>
      </ResponsiveContainer>
    </ChartCard>
  )
}

/** Three small multiples on their own axes: never two scales on one plot. */
export default function HardwareCharts({ points, gpuMemLabel }: Props) {
  const data = (points ?? []).map((p) => ({ t: new Date(p.t).getTime(), cpu: p.cpu, gpu: p.gpu, mem_used: p.mem_used, gpu_mem: p.gpu_mem }))
  const pct = (v: number) => formatPct(v)
  return (
    <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-4">
      <Single title="CPU" data={data} dataKey="cpu" format={pct} domain={[0, 100]} />
      <Single title="GPU" data={data} dataKey="gpu" format={pct} domain={[0, 100]} />
      <Single title="memory used" data={data} dataKey="mem_used" format={(v) => formatBytes(v)} />
      <Single title={gpuMemLabel} data={data} dataKey="gpu_mem" format={(v) => formatBytes(v)} />
    </div>
  )
}
