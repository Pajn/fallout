import { createStore } from "../lib/state";

interface CounterState {
  count: number;
  step: number;
  increment: () => void;
}

export const TITLE = "Custom create";

export const useCounter = createStore<CounterState>("counter", (set) => ({
  count: 0,
  step: 1,
  increment: () => set((state) => ({ count: state.count + state.step })),
}));
