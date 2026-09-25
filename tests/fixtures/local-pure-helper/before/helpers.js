function leaf(value) { return { value, version: 1 }; }
function make(value) { return leaf(value); }
export const selected = make("selected");
export const sibling = 0;
