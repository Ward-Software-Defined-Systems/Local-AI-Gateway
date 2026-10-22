import { create } from 'zustand'

export interface RangePreset {
  key: string
  label: string
  seconds: number
}

export const RANGES: RangePreset[] = [
  { key: '15m', label: '15 min', seconds: 15 * 60 },
  { key: '1h', label: '1 hour', seconds: 3600 },
  { key: '6h', label: '6 hours', seconds: 6 * 3600 },
  { key: '24h', label: '24 hours', seconds: 24 * 3600 },
  { key: '7d', label: '7 days', seconds: 7 * 86_400 },
]

interface RangeState {
  key: string
  set: (key: string) => void
}

/** The one time range every chart on a page shares. */
export const useRange = create<RangeState>()((set) => ({
  key: '1h',
  set: (key) => set({ key }),
}))

/** `from` as RFC 3339, rounded to the minute so react-query keys stay stable between renders. */
export function rangeFrom(key: string, now: Date = new Date()): string {
  const preset = RANGES.find((r) => r.key === key) ?? RANGES[1]
  const ms = Math.floor(now.getTime() / 60_000) * 60_000 - preset.seconds * 1000
  return new Date(ms).toISOString()
}
