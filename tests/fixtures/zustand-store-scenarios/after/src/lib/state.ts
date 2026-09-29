import { create, type StateCreator } from "zustand";

// The app's own entry to Zustand, re-exported for every store.
export { create } from "zustand";

export const stores: string[] = [];
export const events: string[] = [];

// Every store made here is listed by name, for debugging.
export function createStore<T>(name: string, creator: StateCreator<T>) {
  stores.push(name);
  return create<T>()(creator);
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
