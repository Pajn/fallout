const makeState = () => ({ items: [] as number[] });
let state = makeState();
export function reset() { state = makeState(); }

// Hands the array to whoever calls it, inside an array literal.
export function lists() {
  return [state.items];
}

export const hasItems = () => state.items.length > 0;
