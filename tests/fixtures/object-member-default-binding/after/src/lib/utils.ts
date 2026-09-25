function formatDate(date: Date) {
  return date.toISOString();
}

function formatPrice(cents: number) {
  return `$${(cents / 100).toFixed(2)}`;
}

const utils = { formatDate, formatPrice };

export default utils;
