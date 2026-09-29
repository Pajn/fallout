import { base } from './tokens'

function scaled(n: number) {
  return { base, n }
}

export const label = 'spacing'

export const gap = (n: number) => scaled(n)
