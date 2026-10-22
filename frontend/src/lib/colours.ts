// The validated dark categorical steps (data-viz reference palette) and the fixed status scale, as hex for
// SVG attributes; the same values live as CSS variables in index.css.
export const SERIES = ['#3987e5', '#d95926', '#199e70', '#c98500', '#d55181', '#008300', '#9085e9', '#e66767'] as const
export const SERIES_OTHER = '#6b6b66'
export const STATUS = { good: '#0ca30c', warning: '#fab219', serious: '#ec835a', critical: '#d03b3b' } as const
export const SURFACE = '#18181b'
export const GRID = '#2c2c2a'
export const AXIS = '#383835'
export const MUTED = '#898781'
export const TEXT_SECONDARY = '#a1a1aa'

/** The TUIs' thresholds: at or above 80 % critical, 50 % warning, otherwise good. */
export function pctClass(pct: number | null | undefined): string {
  if (pct === null || pct === undefined || !Number.isFinite(pct)) return 'ink-muted'
  if (pct >= 80) return 'ink-critical'
  if (pct >= 50) return 'ink-warning'
  return 'ink-good'
}

/** ANE above 100 mW is worth a glance. */
export function aneClass(mw: number | null | undefined): string {
  if (mw === null || mw === undefined) return 'ink-muted'
  return mw > 100 ? 'ink-warning' : 'ink-good'
}

export function stateClass(state: 'loaded' | 'not-loaded' | 'cloud'): string {
  return state === 'loaded' ? 'ink-good' : state === 'cloud' ? 'ink-cloud' : 'ink-muted'
}

/** Series slot by first appearance, fixed for the life of the page so a filter never repaints survivors. */
export class SlotAssigner {
  private slots = new Map<string, number>()
  slot(key: string): string {
    let i = this.slots.get(key)
    if (i === undefined) {
      i = this.slots.size
      this.slots.set(key, i)
    }
    return i < SERIES.length ? SERIES[i] : SERIES_OTHER
  }
}

/** Critical and high read as the two alarm steps, medium as a warning, low as muted. */
export function severityClass(s: 'low' | 'medium' | 'high' | 'critical'): string {
  return s === 'critical' ? 'ink-critical' : s === 'high' ? 'ink-serious' : s === 'medium' ? 'ink-warning' : 'ink-muted'
}
