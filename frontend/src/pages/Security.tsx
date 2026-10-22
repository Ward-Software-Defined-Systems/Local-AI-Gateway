import Panel from '../components/Panel'
import AlertCentre from '../components/security/AlertCentre'
import FindingsList from '../components/security/FindingsList'
import RulesMatrix from '../components/security/RulesMatrix'
import SummaryTiles from '../components/security/SummaryTiles'
import TestBox from '../components/security/TestBox'

export default function Security() {
  return (
    <div className="space-y-4">
      <SummaryTiles />
      <Panel title="alerts">
        <AlertCentre />
      </Panel>
      <Panel title="findings">
        <FindingsList />
      </Panel>
      <div className="grid gap-4 xl:grid-cols-2">
        <Panel title="test a body against the policy">
          <TestBox />
        </Panel>
        <Panel title="rules and policy">
          <RulesMatrix />
        </Panel>
      </div>
    </div>
  )
}
