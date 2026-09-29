import * as zustand from "zustand";

interface CounterState {
  count: number;
  step: number;
  increment: () => void;
}

export const TITLE = "Namespace";

export const useCounter = zustand.create<CounterState>((set) => ({
  count: 0,
  step: 2,
  increment: () => set((state) => ({ count: state.count + state.step })),
}));
