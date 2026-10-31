let mode = 'light'

export function setMode(next: string) {
  mode = next.trim()
}

export function readMode() {
  return mode
}

setMode('dark')
