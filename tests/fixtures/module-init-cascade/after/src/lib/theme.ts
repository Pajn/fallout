import { warn } from './logger'

export const theme = { badge: { color: 'blue' } }

export function checkTheme(name: string) {
  if (!(name in theme)) {
    warn(name)
  }
}
