const makeState = () => ({ items: [] as number[] });
let state = makeState();
export function reset() { state = makeState(); }

// Pushes onto the array through a local alias of it.
export function add(item: number) {
  const items = state.items;
  items.push(item * 2);
}

export const hasItems = () => state.items.length > 0;
