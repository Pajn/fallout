import { create as createZustand, type StateCreator } from "zustand";

// Zustand's own `create`, re-exported under the name the app's stores use.
export { create as createBoundStore } from "zustand";

export const stores: unknown[] = [];
export const events: string[] = [];

// Every store made here is listed by name, for debugging.
export function createStore<T>(name: string, creator: StateCreator<T>) {
  stores.push(name);
  return createZustand<T>()(creator);
}

// Stands in for Zustand where the app runs without React, and records each store
// it makes. `src/shim/tsconfig.json` maps `zustand` here.
export function create<T>(creator: (set: (partial: Partial<T>) => void) => T) {
  const state = creator(() => {});
  stores.push(state);
  return () => state;
}

export function track(name: string) {
  return events.push(name);
}

export function registerFeature(name: string) {
  events.push(`feature:${name}`);
}

export function readSavedCount() {
  return Number(localStorage.getItem("counter") ?? 0);
}
