function formatDate(date) {
  return date.toISOString();
}

function formatPrice(cents) {
  return `$${(cents / 100).toFixed(2)}`;
}

exports.utils = { formatDate, formatPrice };
