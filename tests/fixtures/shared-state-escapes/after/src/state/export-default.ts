const state = { items: [] as number[], count: 0 };

// Hands the array to every importer, each free to change it.
export default state.items || [];

export const hasItems = () => state.items.length > 0;
