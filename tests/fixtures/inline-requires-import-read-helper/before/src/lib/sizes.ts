import { base } from './tokens'

function scaled(n: number) {
  return { base, n }
}

export const label = 'sizes'

export const small = scaled(2)
