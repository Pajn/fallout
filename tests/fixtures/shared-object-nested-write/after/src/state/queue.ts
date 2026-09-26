const state = { items: [] as string[], label: "Queue" };

export const enqueue = (item: string) => {
  state.items.unshift(item);
};

export const size = () => state.items.length;
