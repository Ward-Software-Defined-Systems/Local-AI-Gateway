import { useState, type ReactNode } from 'react'

export interface TableView {
  columns: string[]
  rows: (string | number)[][]
}

interface Props {
  title: string
  subtitle?: string
  /** The accessibility twin of the chart. */
  table: TableView
  loading?: boolean
  empty?: boolean
  children: ReactNode
}

/** A chart with its title, a table-view toggle, and the previous render held at reduced opacity on refetch. */
export default function ChartCard({ title, subtitle, table, loading = false, empty = false, children }: Props) {
  const [showTable, setShowTable] = useState(false)
  return (
    <section className="rounded border border-zinc-800 bg-zinc-900">
      <div className="flex items-center gap-2 border-b border-zinc-800 px-3 py-1.5">
        <h2 className="text-xs uppercase tracking-wider text-zinc-400">{title}</h2>
        {subtitle && <span className="text-xs text-zinc-500">{subtitle}</span>}
        <button
          type="button"
          onClick={() => setShowTable((v) => !v)}
          className="ml-auto rounded px-1.5 py-0.5 text-xs text-zinc-400 hover:bg-zinc-800 hover:text-zinc-200"
          aria-pressed={showTable}
        >
          {showTable ? 'chart' : 'table'}
        </button>
      </div>
      <div className={`p-2 transition-opacity ${loading ? 'opacity-60' : ''}`}>
        {showTable ? (
          <div className="max-h-56 overflow-auto">
            <table className="w-full font-mono text-xs">
              <thead className="sticky top-0 bg-zinc-900 text-left text-zinc-500">
                <tr>
                  {table.columns.map((c) => (
                    <th key={c} className="pb-1 pr-3 font-normal">
                      {c}
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {table.rows.map((row, i) => (
                  <tr key={i} className="border-t border-zinc-800/60">
                    {row.map((cell, j) => (
                      <td key={j} className={`py-0.5 pr-3 tabular-nums ${j === 0 ? 'text-zinc-400' : 'text-right'}`}>
                        {cell}
                      </td>
                    ))}
                  </tr>
                ))}
                {table.rows.length === 0 && (
                  <tr>
                    <td className="py-2 text-zinc-500" colSpan={table.columns.length}>
                      No data in this range.
                    </td>
                  </tr>
                )}
              </tbody>
            </table>
          </div>
        ) : empty ? (
          <p className="flex h-48 items-center justify-center text-sm text-zinc-500">No data in this range.</p>
        ) : (
          children
        )}
      </div>
    </section>
  )
}
