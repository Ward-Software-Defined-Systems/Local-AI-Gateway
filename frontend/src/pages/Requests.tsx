import { useEffect, useMemo, useState } from 'react'
import { useParams } from 'react-router'
import { useRequestList, type RequestListParams } from '../api/hooks'
import Panel from '../components/Panel'
import Filters, { EMPTY_FILTERS, REQUEST_RANGES, type FilterState } from '../components/requests/Filters'
import RequestDetail from '../components/requests/RequestDetail'
import RequestsTable from '../components/requests/RequestsTable'

function useDebounced<T>(value: T, ms: number): T {
  const [debounced, setDebounced] = useState(value)
  useEffect(() => {
    const timer = setTimeout(() => setDebounced(value), ms)
    return () => clearTimeout(timer)
  }, [value, ms])
  return debounced
}

function toParams(f: FilterState): RequestListParams {
  const preset = REQUEST_RANGES.find((r) => r.key === f.range)
  const from = preset?.seconds ? new Date(Math.floor(Date.now() / 60_000) * 60_000 - preset.seconds * 1000).toISOString() : undefined
  return {
    backend: f.backend || undefined,
    outcome: f.outcome || undefined,
    format: f.format || undefined,
    model: f.model.trim() || undefined,
    client: f.client.trim() || undefined,
    q: f.q.trim() || undefined,
    cloud: f.cloud ? true : undefined,
    has_findings: f.hasFindings ? true : undefined,
    include_passthrough: f.includePassthrough ? true : undefined,
    from,
  }
}

export default function Requests() {
  const { '*': splat } = useParams()
  const selected = splat?.split('/')[0] || undefined
  const [filters, setFilters] = useState<FilterState>(EMPTY_FILTERS)
  const debounced = useDebounced(filters, 300)
  const params = useMemo(() => toParams(debounced), [debounced])
  const list = useRequestList(params)
  const rows = useMemo(() => list.data?.pages.flatMap((p) => p.items) ?? [], [list.data])

  return (
    <div className="space-y-4">
      <Panel title="requests">
        <div className="mb-3">
          <Filters value={filters} onChange={setFilters} />
        </div>
        {list.isError && <p className="text-sm text-red-300">{(list.error as Error).message}</p>}
        <div className={`max-h-[28rem] overflow-auto transition-opacity ${list.isFetching && !list.isFetchingNextPage ? 'opacity-70' : ''}`}>
          <RequestsTable rows={rows} selected={selected} />
        </div>
        <div className="mt-2 flex items-center gap-3 text-xs text-zinc-500">
          <span>
            {rows.length} loaded{list.hasNextPage ? ', more available' : ''}
          </span>
          {list.hasNextPage && (
            <button type="button" onClick={() => void list.fetchNextPage()} disabled={list.isFetchingNextPage} className="rounded px-2 py-1 text-zinc-300 hover:bg-zinc-800 disabled:opacity-50">
              {list.isFetchingNextPage ? 'loading…' : 'load more'}
            </button>
          )}
        </div>
      </Panel>
      {selected ? (
        <Panel title={`request ${selected}`}>
          <RequestDetail key={selected} id={selected} />
        </Panel>
      ) : (
        <p className="text-sm text-zinc-500">Select a request to inspect its bodies, headers, timeline and findings.</p>
      )}
    </div>
  )
}
