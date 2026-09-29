const makeState = () => ({ items: [] as number[] });
let state = makeState();
export function reset() { state = makeState(); }

// Hands the array to whoever drives the generator.
export function* each(_reason?: string) {
  yield state.items;
}

export const hasItems = () => state.items.length > 0;
