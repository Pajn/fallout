const prefix = createPrefix()

function createPrefix() {
  globalThis.loggerReady = true
  return '[%s]'
}

export function warn(name: string) {
  console.warn(prefix, name)
}
