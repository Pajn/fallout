import type { StateCreator } from "zustand";

export const events: string[] = [];

export function track(name: string) {
  return events.push(name);
}

// The app's own middleware, which records every store it wraps as it is made.
export function logged<T>(creator: StateCreator<T>): StateCreator<T> {
  return (set, get, api) => {
    events.push("store-created");
    return creator(set, get, api);
  };
}
