function formatDate(date: Date) {
  return date.toISOString();
}

export const utils = {
  formatDate,
  formatPrice: (cents: number) => `$${cents / 100}`,
};
