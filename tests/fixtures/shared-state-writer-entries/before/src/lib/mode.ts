let mode = 'light'

export function setMode(next: string) {
  mode = next
}

export function readMode() {
  return mode
}

setMode('dark')
