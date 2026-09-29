function makeCache() {
  document.title = "Loading";
  return new Map<string, number>();
}

export const cache = makeCache();

export const TITLE = "Made";

export function reset() {
  cache.set("a", 2);
}
