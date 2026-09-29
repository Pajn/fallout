const cache = new Map<string, number>();

export function reset() {
  cache.set("a", 2);
}

export { cache as c };
