import { create } from "zustand/react";

interface CounterState {
  count: number;
  step: number;
  increment: () => void;
}

export const TITLE = "React subpath";

export const useCounter = create<CounterState>((set) => ({
  count: 0,
  step: 1,
  increment: () => set((state) => ({ count: state.count + state.step })),
}));
