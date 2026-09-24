function formatDate(date: Date) {
  return date.toISOString();
}

function formatPrice(cents: number) {
  return `$${cents / 100}`;
}

export default { formatDate, formatPrice };
