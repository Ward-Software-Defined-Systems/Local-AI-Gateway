import { NavLink, Route, Routes } from 'react-router'
import StatusBar from './components/StatusBar'
import BackendPage from './pages/BackendPage'
import Overview from './pages/Overview'
import Requests from './pages/Requests'
import Security from './pages/Security'
import Settings from './pages/Settings'
import { useGatewaySocket } from './ws/useGatewaySocket'

const TABS = [
  { to: '/', label: 'Overview', end: true },
  { to: '/lmstudio', label: 'LM Studio' },
  { to: '/ollama', label: 'Ollama' },
  { to: '/requests', label: 'Requests' },
  { to: '/security', label: 'Security' },
  { to: '/settings', label: 'Settings' },
]

export default function App() {
  useGatewaySocket()
  return (
    <div className="min-h-screen bg-zinc-950 text-zinc-200">
      <StatusBar />
      <nav className="border-b border-zinc-800 px-4">
        <ul className="flex gap-1">
          {TABS.map((tab) => (
            <li key={tab.to}>
              <NavLink
                to={tab.to}
                end={tab.end}
                className={({ isActive }) =>
                  `inline-block border-b-2 px-3 py-2 text-sm ${
                    isActive
                      ? 'border-cyan-400 text-zinc-50'
                      : 'border-transparent text-zinc-400 hover:text-zinc-200'
                  }`
                }
              >
                {tab.label}
              </NavLink>
            </li>
          ))}
        </ul>
      </nav>
      <main className="mx-auto max-w-screen-2xl p-4">
        <Routes>
          <Route path="/" element={<Overview />} />
          <Route path="/lmstudio" element={<BackendPage backend="lmstudio" />} />
          <Route path="/ollama" element={<BackendPage backend="ollama" />} />
          <Route path="/requests/*" element={<Requests />} />
          <Route path="/security/*" element={<Security />} />
          <Route path="/settings" element={<Settings />} />
          <Route path="*" element={<p className="text-zinc-400">Nothing here.</p>} />
        </Routes>
      </main>
    </div>
  )
}
