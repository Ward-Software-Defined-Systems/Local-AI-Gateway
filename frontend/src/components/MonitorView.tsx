import { useEffect, useMemo, useState } from 'react'
import { useCost, useHardwareSeries, useModels, useRecentRequests, useRolling, useSeries, useStatus } from '../api/hooks'
import type { Backend } from '../api/types'
import { SERIES, STATUS } from '../lib/colours'
import { formatCompact, formatMs, formatTps, formatUsd } from '../lib/format'
import { rangeFrom, useRange } from '../lib/range'
import { useLive } from '../store/live'
import CostTable from './CostTable'
import HardwareLine from './HardwareLine'
import LiveFeed from './LiveFeed'
import ModelsTable from './ModelsTable'
import Panel from './Panel'
import RollingMetrics from './RollingMetrics'
import Toolbar from './Toolbar'
import HardwareCharts from './charts/HardwareCharts'
import LineSeriesChart from './charts/LineSeriesChart'
import StackedTokensChart from './charts/StackedTokensChart'

/** Re-renders once a minute so the range's `from` moves with the clock. */
function useMinuteTick(): number {
  const [tick, setTick] = useState(0)
  useEffect(() => {
    const timer = setInterval(() => setTick((n) => n + 1), 60_000)
    return () => clearInterval(timer)
  }, [])
  return tick
}

/** The Overview (no backend) and the per-backend tabs share this composition. */
export default function MonitorView({ backend }: { backend?: Backend }) {
  const status = useStatus()
  const platform = status.data?.platform
  const feed = useLive((s) => s.feed)
  const inflightMap = useLive((s) => s.inflight)
  const hardware = useLive((s) => s.hardware)
  const liveRolling = useLive((s) => s.rolling)
  const lastModel = useLive((s) => s.lastModel)
  const seedFeed = useLive((s) => s.seedFeed)
  const recent = useRecentRequests()
  useEffect(() => {
    if (recent.data) seedFeed(recent.data.items)
  }, [recent.data, seedFeed])

  const models = useModels(backend)
  const rollingQuery = useRolling(backend)
  const rolling = backend ? rollingQuery.data : liveRolling ?? rollingQuery.data
  const sessionStart = rolling?.session_started_at ?? status.data?.started_at
  const cost = useCost(sessionStart, backend)

  const rangeKey = useRange((s) => s.key)
  const tick = useMinuteTick()
  const from = useMemo(() => rangeFrom(rangeKey), [rangeKey, tick])
  const req = useSeries('req_per_min', from, backend)
  const tps = useSeries('tok_per_sec', from, backend)
  const ttft = useSeries('ttft', from, backend)
  const byModel = useSeries('tokens_by_model', from, backend)
  const costCumulative = useSeries('cost_cumulative', from, backend)
  const hardwareSeries = useHardwareSeries(from)

  const rows = useMemo(() => (backend ? feed.filter((r) => r.backend === backend) : feed), [feed, backend])
  const inflight = useMemo(() => Object.values(inflightMap), [inflightMap])
  const costKeys = useMemo(() => {
    const keys = new Set<string>()
    for (const p of costCumulative.data?.points ?? []) for (const k of Object.keys(p.values)) keys.add(k)
    const order = cost.data?.columns ?? []
    return [...keys].sort((a, b) => (order.indexOf(a) === -1 ? 99 : order.indexOf(a)) - (order.indexOf(b) === -1 ? 99 : order.indexOf(b)))
  }, [costCumulative.data, cost.data])

  return (
    <div className="space-y-4">
      <div className="grid gap-4 xl:grid-cols-3">
        <div className="xl:col-span-2">
          <Panel title={`live request feed (${rows.length}/200)`}>
            <LiveFeed rows={rows} inflight={inflight} models={models.data ?? []} showBackend={!backend} backend={backend} />
          </Panel>
        </div>
        <div className="space-y-4">
          <Panel title="rolling metrics">
            <RollingMetrics metrics={rolling} />
          </Panel>
          <Panel title="hypothetical session cost">
            <CostTable cost={cost.data} />
          </Panel>
        </div>
      </div>
      <Panel title="hardware">
        <HardwareLine sample={hardware} platform={platform} backend={backend} />
      </Panel>
      <Panel title={backend ? 'models' : 'models (both backends)'}>
        <ModelsTable models={models.data ?? []} showBackend={!backend} lastModel={lastModel} />
      </Panel>
      <Toolbar />
      <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
        <LineSeriesChart
          title="requests per bucket"
          series={req.data}
          loading={req.isFetching}
          format={formatCompact}
          lines={[
            { key: 'requests', label: 'requests', color: SERIES[0] },
            { key: 'failed', label: 'failed', color: STATUS.critical },
          ]}
        />
        <LineSeriesChart
          title="tokens per second"
          series={tps.data}
          loading={tps.isFetching}
          format={formatTps}
          lines={[
            { key: 'mean', label: 'mean', color: SERIES[0] },
            { key: 'p95', label: 'p95', color: SERIES[1] },
          ]}
        />
        <LineSeriesChart
          title="time to first token"
          series={ttft.data}
          loading={ttft.isFetching}
          format={formatMs}
          lines={[
            { key: 'p50', label: 'p50', color: SERIES[0] },
            { key: 'p95', label: 'p95', color: SERIES[1] },
          ]}
        />
        <StackedTokensChart title="tokens by model" series={byModel.data} loading={byModel.isFetching} />
        <div className="md:col-span-2">
          <LineSeriesChart
            title="cumulative hypothetical cost"
            series={costCumulative.data}
            loading={costCumulative.isFetching}
            format={formatUsd}
            lines={costKeys.map((k, i) => ({ key: k, label: k, color: SERIES[i % SERIES.length] }))}
          />
        </div>
      </div>
      <HardwareCharts points={hardwareSeries.data} loading={hardwareSeries.isFetching} gpuMemLabel={platform === 'linux' ? 'VRAM used' : 'GPU memory in use'} />
    </div>
  )
}
