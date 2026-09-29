// Stands in for Zustand where the app runs without React, and records each store
// it makes.
export const stores: unknown[] = [];

export function create<T>(creator: (set: (partial: Partial<T>) => void) => T) {
  const state = creator(() => {});
  stores.push(state);
  return () => state;
}
