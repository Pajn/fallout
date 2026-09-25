import { readPrice } from "./price";

function formatPrice(cents: number) {
  return `$${(cents / 100).toFixed(2)}`;
}

export const utils = { readPrice, formatPrice };
