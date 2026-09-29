const cache = new Map<string, number>();

export function reset() {
  cache.set("a", 1);
}

export { cache as c };
