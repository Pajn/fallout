const cache = new Map<string, { count: number }>();

export const isCached = (key: string) => cache.has(key);

export const lookup = (key: string) => cache.get(key.trim());

const list: { id: string }[] = [];

export const hasId = (id: string) => list.some((entry) => entry.id === id);

export const firstWith = (id: string) => list.find((entry) => entry.id === id.trim());

const items: { label: string }[] = [];

export const hasLabel = (label: string) => items.some((item) => item.label === label);

const mutate = (item: { label: string }) => {
  item.label = item.label.trim().toUpperCase();
};

export const shout = () => items.forEach(mutate);

let queue: string[] = [];

export const inQueue = (entry: string) => queue.includes(entry);

export const queued = (entry: string) => queue.indexOf(entry.trim()) >= 0;

export const flush = () => {
  queue = [];
};
