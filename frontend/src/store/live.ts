import { create } from 'zustand'
import type {
  Backend,
  BackendSnapshot,
  HardwareSample,
  HelloMessage,
  InflightRequest,
  RequestRecord,
  RollingMetrics,
  ServerMessage,
} from '../api/types'

export type Connection = 'connecting' | 'open' | 'closed'

/** The feed keeps this many completed rows. */
export const FEED_CAPACITY = 200

interface LiveState {
  connection: Connection
  server: HelloMessage | null
  feed: RequestRecord[]
  seeded: boolean
  inflight: Record<string, InflightRequest>
  backends: Partial<Record<Backend, BackendSnapshot>>
  hardware: HardwareSample | null
  rolling: RollingMetrics | null
  paused: boolean
  /** Rows that arrived while paused; they never enter the feed, as in the TUIs. */
  skippedWhilePaused: number
  lastModel: Partial<Record<Backend, string>>
  /** Sections a reload could not apply; cleared by a restart (the hello resets it). */
  restartRequired: string[]
  /** Alerts that arrived over the socket since the page loaded. */
  liveAlerts: number
  setConnection: (connection: Connection) => void
  handle: (message: ServerMessage) => void
  seedFeed: (rows: RequestRecord[]) => void
  togglePause: () => void
  resetFeed: () => void
}

function isFeedRow(record: RequestRecord): boolean {
  return record.kind === 'http' && record.api_format !== 'passthrough'
}

export const useLive = create<LiveState>()((set) => ({
  connection: 'connecting',
  server: null,
  feed: [],
  seeded: false,
  inflight: {},
  backends: {},
  hardware: null,
  rolling: null,
  paused: false,
  skippedWhilePaused: 0,
  lastModel: {},
  restartRequired: [],
  liveAlerts: 0,
  setConnection: (connection) => set({ connection }),
  seedFeed: (rows) =>
    set((s) => {
      if (s.seeded) return {}
      const known = new Set(s.feed.map((r) => r.id))
      const merged = [...s.feed, ...rows.filter((r) => !known.has(r.id) && isFeedRow(r))]
      merged.sort((a, b) => (a.ended_at < b.ended_at ? 1 : -1))
      return { feed: merged.slice(0, FEED_CAPACITY), seeded: true }
    }),
  togglePause: () => set((s) => ({ paused: !s.paused })),
  resetFeed: () => set({ feed: [], skippedWhilePaused: 0, lastModel: {} }),
  handle: (message) =>
    set((s) => {
      switch (message.type) {
        case 'hello': {
          const inflight: Record<string, InflightRequest> = {}
          for (const r of message.inflight) inflight[r.id] = r
          return { server: message, inflight, restartRequired: s.server && s.server.started_at !== message.started_at ? [] : s.restartRequired }
        }
        case 'config_changed':
          return { restartRequired: message.restart_required }
        case 'alert':
          return { liveAlerts: s.liveAlerts + 1 }
        case 'request_started':
          return { inflight: { ...s.inflight, [message.request.id]: message.request } }
        case 'request_completed': {
          const inflight = { ...s.inflight }
          delete inflight[message.record.id]
          const record = message.record
          if (!isFeedRow(record)) return { inflight }
          if (s.paused) return { inflight, skippedWhilePaused: s.skippedWhilePaused + 1 }
          const model = record.model_resolved ?? record.model_requested
          return {
            inflight,
            feed: [record, ...s.feed.filter((r) => r.id !== record.id)].slice(0, FEED_CAPACITY),
            lastModel: model ? { ...s.lastModel, [record.backend]: model } : s.lastModel,
          }
        }
        case 'backend_snapshot':
          return { backends: { ...s.backends, [message.snapshot.backend]: message.snapshot } }
        case 'hardware':
          return { hardware: message.sample }
        case 'metrics':
          return { rolling: message.rolling }
        default:
          return {}
      }
    }),
}))
