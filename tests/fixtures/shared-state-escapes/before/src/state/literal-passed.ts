const state = { items: [] as number[], count: 0 };

declare function sink(payload: { items: number[] }): void;

// Hands the array to `sink` inside an object literal, and `sink` is free to
// change it.
export function send() {
  sink({ items: state.items });
}

export const hasItems = () => state.items.length > 0;
