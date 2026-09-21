const prefix = createPrefix()

function createPrefix() {
  return '[%s]'
}

export function warn(name: string) {
  console.warn(prefix, name)
}
