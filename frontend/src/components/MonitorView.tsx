import { useEffect, useMemo, useState } from 'react'
import { useCost, useHardwareSeries, useModels, useRecentRequests, useRolling, useSeries, useSpend, useStatus, type Scope } from '../api/hooks'
import { BACKEND_NAMES, CLOUD_BACKENDS, LOCAL_BACKENDS, type Backend } from '../api/types'
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
import SpendTable from './SpendTable'
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

/** One fixed colour per provider, so a provider keeps its colour whatever else is on the chart. */
const PROVIDER_COLOURS: Record<string, string> = { anthropic: SERIES[0], gemini: SERIES[1], openai: SERIES[2] }

/** The Overview (no scope), the per-backend tabs and the Cloud tab share this composition. */
export default function MonitorView({ scope }: { scope?: Scope }) {
  const backend = scope?.backend
  const group = scope?.group
  const cloud = group === 'cloud'
  const include = useMemo(
    () => (b: Backend) => (backend ? b === backend : group ? (group === 'cloud' ? CLOUD_BACKENDS : LOCAL_BACKENDS).includes(b) : true),
    [backend, group],
  )
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
  const rollingQuery = useRolling(scope)
  const rolling = scope ? rollingQuery.data : liveRolling ?? rollingQuery.data
  const sessionStart = rolling?.session_started_at ?? status.data?.started_at
  const cost = useCost(cloud ? undefined : sessionStart, scope)
  const spend = useSpend(cloud ? sessionStart : undefined)

  const rangeKey = useRange((s) => s.key)
  const tick = useMinuteTick()
  const from = useMemo(() => rangeFrom(rangeKey), [rangeKey, tick])
  const req = useSeries('req_per_min', from, scope)
  const tps = useSeries('tok_per_sec', from, scope)
  const ttft = useSeries('ttft', from, scope)
  const byModel = useSeries('tokens_by_model', from, scope)
  const cumulative = useSeries(cloud ? 'spend_cumulative' : 'cost_cumulative', from, scope)
  const hardwareSeries = useHardwareSeries(from)

  const rows = useMemo(() => feed.filter((r) => include(r.backend)), [feed, include])
  const inflight = useMemo(() => Object.values(inflightMap), [inflightMap])
  const cumulativeLines = useMemo(() => {
    if (cloud) return CLOUD_BACKENDS.map((b) => ({ key: b, label: BACKEND_NAMES[b], color: PROVIDER_COLOURS[b] }))
    const keys = new Set<string>()
    for (const p of cumulative.data?.points ?? []) for (const k of Object.keys(p.values)) keys.add(k)
    const order = cost.data?.columns ?? []
    return [...keys]
      .sort((a, b) => (order.indexOf(a) === -1 ? 99 : order.indexOf(a)) - (order.indexOf(b) === -1 ? 99 : order.indexOf(b)))
      .map((k, i) => ({ key: k, label: k, color: SERIES[i % SERIES.length] }))
  }, [cloud, cumulative.data, cost.data])

  return (
    <div className="space-y-4">
      <div className="grid gap-4 xl:grid-cols-3">
        <div className="xl:col-span-2">
          <Panel title={`live request feed (${rows.length}/200)`}>
            <LiveFeed rows={rows} inflight={inflight} models={models.data ?? []} showBackend={!backend} include={include} />
          </Panel>
        </div>
        <div className="space-y-4">
          <Panel title="rolling metrics">
            <RollingMetrics metrics={rolling} />
          </Panel>
          {cloud ? (
            <Panel title="spend at list price (session)">
              <SpendTable spend={spend.data} />
            </Panel>
          ) : (
            <Panel title="hypothetical session cost">
              <CostTable cost={cost.data} />
            </Panel>
          )}
        </div>
      </div>
      {!cloud && (
        <Panel title="hardware">
          <HardwareLine sample={hardware} platform={platform} backend={backend} />
        </Panel>
      )}
      {!cloud && (
        <Panel title={backend ? 'models' : 'models (local backends)'}>
          <ModelsTable models={models.data ?? []} showBackend={!backend} lastModel={lastModel} />
        </Panel>
      )}
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
            title={cloud ? 'cumulative spend by provider' : 'cumulative hypothetical cost'}
            series={cumulative.data}
            loading={cumulative.isFetching}
            format={formatUsd}
            lines={cumulativeLines}
          />
        </div>
      </div>
      {!cloud && <HardwareCharts points={hardwareSeries.data} loading={hardwareSeries.isFetching} gpuMemLabel={platform === 'linux' ? 'VRAM used' : 'GPU memory in use'} />}
    </div>
  )
}
