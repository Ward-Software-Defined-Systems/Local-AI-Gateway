import type { Backend } from '../api/types'
import MonitorView from '../components/MonitorView'

export default function BackendPage({ backend }: { backend: Backend }) {
  return <MonitorView key={backend} scope={{ backend }} />
}
