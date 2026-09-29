import { create } from "zustand";
import { track } from "../lib/state";

interface CounterState {
  count: number;
  step: number;
  openedAt?: number;
  increment: () => void;
}

export const TITLE = "Effect removed";

export const useCounter = create<CounterState>((set) => ({
  count: 0,
  step: 1,
  increment: () => {
    track("counter-incremented");
    set((state) => ({ count: state.count + state.step }));
  },
}));
