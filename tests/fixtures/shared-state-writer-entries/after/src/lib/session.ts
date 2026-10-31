let ready = false

export const session = {
  start() {
    ready = !ready
  },
}

export function isReady() {
  return ready
}
