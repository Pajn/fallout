import { create as createStore } from "zustand";

interface CounterState {
  count: number;
  step: number;
  increment: () => void;
}

export const TITLE = "Alias";

export const useCounter = createStore<CounterState>((set) => ({
  count: 0,
  step: 2,
  increment: () => set((state) => ({ count: state.count + state.step })),
}));
