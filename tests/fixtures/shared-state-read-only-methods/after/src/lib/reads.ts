const cache = new Map<string, string>();

export const isCached = (key: string) => cache.has(key);

export const holds = (key: string, value: string) => cache.get(key.trim()) === value;

export const remember = (key: string, value: string) => {
  cache.set(key, value);
};

const list: string[] = [];

export const hasItem = (item: string) => list.includes(item);

export const findsItem = (item: string) => list.find((entry) => entry === item.trim()) !== undefined;

export const addItem = (item: string) => {
  list.push(item);
};

const seen = new Set<string>();

export const wasSeen = (id: string) => seen.has(id);

export const seenBoth = (a: string, b: string) => a !== b && seen.has(a) && seen.has(b);

export const markSeen = (id: string) => {
  seen.add(id);
};
