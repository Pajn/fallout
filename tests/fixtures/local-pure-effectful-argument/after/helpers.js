function make(value) { return { value, version: 2 }; }
export const selected = make(globalThis.configuration.value);
export const sibling = 0;
