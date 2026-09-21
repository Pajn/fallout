import { theme } from './theme'

export function Badge({ label }: { label: string }) {
  return <span style={theme.badge}>{label}</span>
}
