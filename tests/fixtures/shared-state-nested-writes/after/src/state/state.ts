const makeState = () => ({ items: [] as number[] });
let state = makeState();
export function reset() { state = makeState(); }
export function add() { state.items.push(2); }
export const count = () => state.items.length;
