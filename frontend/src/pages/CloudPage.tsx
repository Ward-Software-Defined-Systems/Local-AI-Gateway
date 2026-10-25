import MonitorView from '../components/MonitorView'

/** Anthropic, Gemini and OpenAI together: their feed, rolling metrics and what they charged. */
export default function CloudPage() {
  return <MonitorView key="cloud" scope={{ group: 'cloud' }} />
}
