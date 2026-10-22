import Panel from '../components/Panel'
import ConfigEditor from '../components/settings/ConfigEditor'
import Maintenance from '../components/settings/Maintenance'
import RuntimePanel from '../components/settings/RuntimePanel'

export default function Settings() {
  return (
    <div className="space-y-4">
      <Panel title="runtime">
        <RuntimePanel />
      </Panel>
      <Panel title="config.toml">
        <ConfigEditor />
      </Panel>
      <Panel title="maintenance">
        <Maintenance />
      </Panel>
    </div>
  )
}
