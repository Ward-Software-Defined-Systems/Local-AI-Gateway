import { Bar, BarChart, CartesianGrid, ReferenceLine, ResponsiveContainer, Tooltip, XAxis, YAxis } from 'recharts'
import type { RequestRecord } from '../../api/types'
import { SERIES, STATUS, SURFACE } from '../../lib/colours'
import { formatBytes } from '../../lib/format'
import { TooltipBox, axisLine, axisTick, gridProps } from '../charts/common'
import type { RechartsTooltipProps } from '../charts/common'

function Tip({ active, label, payload }: RechartsTooltipProps) {
  if (!active || !payload?.length) return null
  const v = payload[0]?.value
  return <TooltipBox title={`${label} ms`} rows={[{ name: 'chunk', value: typeof v === 'number' ? formatBytes(v) : '-', color: SERIES[0] }]} />
}

/** Response bytes over time since the request arrived, with the first-token mark. */
export default function ChunkTimeline({ record }: { record: RequestRecord }) {
  if (record.chunk_timeline.length === 0) return <p className="text-sm text-zinc-500">No chunk timeline recorded.</p>
  const data = record.chunk_timeline.map(([offset, bytes]) => ({ t: offset, bytes }))
  const ttft = record.ttft_ms
  return (
    <div>
      <p className="mb-2 text-xs text-zinc-500">
        {record.chunk_timeline.length} chunk{record.chunk_timeline.length === 1 ? '' : 's'} over {Math.round(record.total_ms)} ms
        {ttft !== null ? ` · first token at ${Math.round(ttft)} ms${record.timings_estimated ? ' (gateway clock)' : ''}` : ''}
      </p>
      <ResponsiveContainer width="100%" height={180}>
        <BarChart data={data} margin={{ top: 8, right: 12, left: 0, bottom: 0 }} maxBarSize={6}>
          <CartesianGrid {...gridProps} />
          <XAxis dataKey="t" type="number" domain={[0, 'dataMax']} tick={axisTick} axisLine={axisLine} tickLine={false} tickFormatter={(v: number) => `${v}ms`} minTickGap={50} />
          <YAxis tick={axisTick} axisLine={false} tickLine={false} width={52} tickFormatter={(v: number) => formatBytes(v)} />
          <Tooltip content={<Tip />} cursor={{ fill: 'rgba(255,255,255,0.04)' }} />
          {ttft !== null && <ReferenceLine x={ttft} stroke={STATUS.warning} strokeWidth={1} />}
          <Bar dataKey="bytes" fill={SERIES[0]} stroke={SURFACE} strokeWidth={1} isAnimationActive={false} />
        </BarChart>
      </ResponsiveContainer>
    </div>
  )
}
