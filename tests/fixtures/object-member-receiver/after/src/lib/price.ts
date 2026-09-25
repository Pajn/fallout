export function readPrice(this: { formatPrice(cents: number): string }, cents: number) {
  return this.formatPrice(cents);
}
