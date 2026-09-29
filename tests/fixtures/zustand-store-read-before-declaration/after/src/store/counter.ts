import { create } from "zustand";

interface CounterState {
  count: number;
  step: number;
  increment: () => void;
}

export const COUNTER_TITLE = "Counter";

export const useCounter = create<CounterState>((set) => ({
  count: INITIAL_COUNT,
  step: 2,
  increment: () => set((state) => ({ count: state.count + state.step })),
}));

const INITIAL_COUNT = 0;
