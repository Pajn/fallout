let ready = false

export const session = {
  start() {
    ready = true
  },
}

export function isReady() {
  return ready
}
