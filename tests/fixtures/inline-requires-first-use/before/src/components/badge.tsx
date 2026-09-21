import { warn } from '../lib/logger'

export function Badge({ label }: { label: string }) {
  if (!label) {
    warn('badge without a label')
  }
  return <span>{label}</span>
}
