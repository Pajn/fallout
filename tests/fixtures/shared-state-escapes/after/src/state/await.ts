const state = { items: [] as number[], count: 0 };

// Awaiting the array calls its `then`, if it has one, with the array as `this`.
export async function settle(_reason?: string) {
  await state.items;
}

export const hasItems = () => state.items.length > 0;
