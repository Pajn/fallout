import { create } from "zustand";

interface CounterState {
  count: number;
  step: number;
  increment: () => void;
}

export const TITLE = "Curried";

export const useCounter = create<CounterState>()((set) => ({
  count: 0,
  step: 1,
  increment: () => set((state) => ({ count: state.count + state.step })),
}));
