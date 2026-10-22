import { useEffect } from 'react'
import { useQueryClient } from '@tanstack/react-query'
import type { ServerMessage } from '../api/types'
import { useLive } from '../store/live'

const PING_EVERY_MS = 25_000
const BACKOFF_MIN_MS = 500
const BACKOFF_MAX_MS = 10_000

/**
 * Keeps one WebSocket to `/ws` open for the life of the app, reconnecting with jittered backoff. The
 * scheme follows the page (wss on https). Events feed the live store; the ones that change what REST
 * serves invalidate the matching queries, and `resync` (the server dropped events for us) refetches all.
 */
export function useGatewaySocket() {
  const queryClient = useQueryClient()
  const setConnection = useLive((s) => s.setConnection)
  const handle = useLive((s) => s.handle)

  useEffect(() => {
    let socket: WebSocket | null = null
    let closed = false
    let attempt = 0
    let reconnectTimer: ReturnType<typeof setTimeout> | undefined
    let pingTimer: ReturnType<typeof setInterval> | undefined

    const connect = () => {
      if (closed) return
      setConnection('connecting')
      const scheme = location.protocol === 'https:' ? 'wss' : 'ws'
      socket = new WebSocket(`${scheme}://${location.host}/ws`)

      socket.onopen = () => {
        attempt = 0
        setConnection('open')
        pingTimer = setInterval(() => socket?.send(JSON.stringify({ type: 'ping' })), PING_EVERY_MS)
      }
      socket.onmessage = (ev) => {
        let msg: unknown
        try {
          msg = JSON.parse(ev.data as string)
        } catch {
          return
        }
        if (typeof msg !== 'object' || msg === null || typeof (msg as { type?: unknown }).type !== 'string') return
        const message = msg as ServerMessage
        handle(message)
        switch (message.type) {
          case 'resync':
            void queryClient.invalidateQueries()
            break
          case 'backend_snapshot':
            void queryClient.invalidateQueries({ queryKey: ['models'] })
            void queryClient.invalidateQueries({ queryKey: ['status'] })
            break
          case 'request_completed':
            void queryClient.invalidateQueries({ queryKey: ['lifetime'] })
            void queryClient.invalidateQueries({ queryKey: ['cost'] })
            if (message.record.finding_count > 0) void queryClient.invalidateQueries({ queryKey: ['findings'] })
            break
          case 'alert':
            void queryClient.invalidateQueries({ queryKey: ['alerts'] })
            void queryClient.invalidateQueries({ queryKey: ['status'] })
            break
          case 'config_changed':
            void queryClient.invalidateQueries({ queryKey: ['dlp-rules'] })
            void queryClient.invalidateQueries({ queryKey: ['config-file'] })
            void queryClient.invalidateQueries({ queryKey: ['status'] })
            break
          default:
            break
        }
      }
      socket.onclose = () => {
        if (pingTimer) clearInterval(pingTimer)
        setConnection('closed')
        if (closed) return
        const delay = Math.min(BACKOFF_MAX_MS, BACKOFF_MIN_MS * 2 ** attempt) * (0.75 + Math.random() / 2)
        attempt += 1
        reconnectTimer = setTimeout(connect, delay)
      }
      socket.onerror = () => socket?.close()
    }

    connect()
    return () => {
      closed = true
      if (reconnectTimer) clearTimeout(reconnectTimer)
      if (pingTimer) clearInterval(pingTimer)
      socket?.close()
    }
  }, [queryClient, setConnection, handle])
}
