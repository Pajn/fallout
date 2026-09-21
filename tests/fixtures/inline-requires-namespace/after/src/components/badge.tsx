const registry = require('../lib/registry')

export function Badge({ label }: { label: string }) {
  return <span data-registered={registry.length}>{label}</span>
}
