import { createStore } from "zustand/vanilla";

interface CounterState {
  count: number;
  step: number;
  increment: () => void;
}

export const TITLE = "Vanilla";

export const counterStore = createStore<CounterState>()((set) => ({
  count: 0,
  step: 1,
  increment: () => set((state) => ({ count: state.count + state.step })),
}));
