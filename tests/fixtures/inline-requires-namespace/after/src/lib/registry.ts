const names: string[] = []

register('badge')

function register(name: string) {
  names.push(name.trim())
}
