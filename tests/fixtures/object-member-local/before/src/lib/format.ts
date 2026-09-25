function formatDate(date: Date) {
  return date.toISOString();
}

function formatPrice(cents: number) {
  return `$${cents / 100}`;
}

const utils = { formatDate, formatPrice };

export const dateLabel = (date: Date) => `On ${utils.formatDate(date)}`;
export const priceLabel = (cents: number) => `For ${utils.formatPrice(cents)}`;
