const state = { items: [] as number[], count: 0 };

// Hands the array to whoever calls it, who is free to change it.
export const items = (_reason?: string) => state.items;

export const hasItems = () => state.items.length > 0;
