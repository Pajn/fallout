import { report } from '#app/lib/reporting'

export default function HomePage() {
  return <button onClick={() => report('clicked')}>Go</button>
}
