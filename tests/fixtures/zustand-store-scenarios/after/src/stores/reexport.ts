import { createBoundStore } from "../lib/state";

interface CounterState {
  count: number;
  step: number;
  increment: () => void;
}

export const TITLE = "Reexport";

export const useCounter = createBoundStore<CounterState>((set) => ({
  count: 0,
  step: 2,
  increment: () => set((state) => ({ count: state.count + state.step })),
}));
