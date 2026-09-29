const cache = new Map<string, string>();

export const isCached = (key: string) => cache.has(key);

export const holds = (key: string, value: string) => cache.get(key) === value;

export const remember = (key: string, value: string) => {
  cache.set(key, value.trim());
};

const list: string[] = [];

export const hasItem = (item: string) => list.includes(item);

export const findsItem = (item: string) => list.find((entry) => entry === item) !== undefined;

export const addItem = (item: string) => {
  list.push(item.trim());
};

const seen = new Set<string>();

export const wasSeen = (id: string) => seen.has(id);

export const seenBoth = (a: string, b: string) => seen.has(a) && seen.has(b);

export const markSeen = (id: string) => {
  seen.add(id.trim());
};
