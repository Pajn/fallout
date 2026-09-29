import { create } from "zustand";

interface CounterState {
  count: number;
  step: number;
  increment: () => void;
}

export const TITLE = "With types";

const createCounter = create.withTypes<CounterState>();

export const useCounter = createCounter((set) => ({
  count: 0,
  step: 1,
  increment: () => set((state) => ({ count: state.count + state.step })),
}));
