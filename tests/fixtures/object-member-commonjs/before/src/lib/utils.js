function formatDate(date) {
  return date.toISOString();
}

function formatPrice(cents) {
  return `$${cents / 100}`;
}

exports.utils = { formatDate, formatPrice };
