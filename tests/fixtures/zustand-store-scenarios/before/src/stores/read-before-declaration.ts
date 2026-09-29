import { create } from "zustand";

interface CounterState {
  count: number;
  step: number;
  increment: () => void;
}

export const TITLE = "Read before declaration";

export const useCounter = create<CounterState>((set) => ({
  count: INITIAL_COUNT,
  step: 1,
  increment: () => set((state) => ({ count: state.count + state.step })),
}));

const INITIAL_COUNT = 0;
