import RangePicker from './RangePicker'

/** The one filter row above the charts it scopes. */
export default function Toolbar() {
  return (
    <div className="mb-4 flex items-center gap-4">
      <RangePicker />
    </div>
  )
}
