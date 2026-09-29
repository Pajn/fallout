const makeState = () => ({ items: [] as number[] });
let state = makeState();
export function reset() { state = makeState(); }

// Hands the array to whoever calls it, who is free to change it.
export function items() {
  return state.items;
}

export const hasItems = () => state.items.length > 0;
