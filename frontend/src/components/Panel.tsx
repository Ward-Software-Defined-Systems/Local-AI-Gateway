import type { ReactNode } from 'react'

export default function Panel({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="rounded border border-zinc-800 bg-zinc-900">
      <h2 className="border-b border-zinc-800 px-3 py-1.5 text-xs uppercase tracking-wider text-zinc-400">
        {title}
      </h2>
      <div className="p-3">{children}</div>
    </section>
  )
}
