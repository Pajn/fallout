import create from "zustand";

interface CounterState {
  count: number;
  step: number;
  increment: () => void;
}

export const TITLE = "Default v4";

export const useCounter = create<CounterState>((set) => ({
  count: 0,
  step: 2,
  increment: () => set((state) => ({ count: state.count + state.step })),
}));
