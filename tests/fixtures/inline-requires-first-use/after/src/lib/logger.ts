const prefix = createPrefix()

function createPrefix() {
  console.log("initializing logger")
  return '[%s] '
}

export function warn(name: string) {
  console.warn(prefix, name)
}
