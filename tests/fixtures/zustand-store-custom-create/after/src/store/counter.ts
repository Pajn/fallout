import { createStore } from "../lib/create-store";

interface CounterState {
  count: number;
  step: number;
  increment: () => void;
}

export const COUNTER_TITLE = "Counter";

export const useCounter = createStore<CounterState>("counter", (set) => ({
  count: 0,
  step: 2,
  increment: () => set((state) => ({ count: state.count + state.step })),
}));
