import { create } from "zustand";
import { track } from "../lib/analytics";

interface CounterState {
  count: number;
  step: number;
  openedAt?: number;
  increment: () => void;
}

export const COUNTER_TITLE = "Counter";

export const useCounter = create<CounterState>((set) => ({
  count: 0,
  step: 1,
  openedAt: track("counter-opened"),
  increment: () => {
    track("counter-incremented");
    set((state) => ({ count: state.count + state.step }));
  },
}));
