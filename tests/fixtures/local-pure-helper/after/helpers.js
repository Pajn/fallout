function leaf(value) { return { value, version: 2 }; }
function make(value) { return leaf(value); }
export const selected = make("selected");
export const sibling = 0;
