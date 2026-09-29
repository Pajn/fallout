import { create, type StateCreator } from "zustand";

export const stores: string[] = [];

// Every store in the app is made here, so that each can be listed for debugging.
export function createStore<T>(name: string, creator: StateCreator<T>) {
  stores.push(name);
  return create<T>()(creator);
}
