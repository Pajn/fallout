import { create } from "zustand";
import { immer } from "zustand/middleware/immer";
import { track } from "../lib/state";

interface CounterState {
  count: number;
  step: number;
  openedAt?: number;
  increment: () => void;
}

export const TITLE = "Immer effect removed";

export const useCounter = create<CounterState>()(
  immer((set) => ({
    count: 0,
    step: 1,
    openedAt: track("counter-opened"),
    increment: () => {
      track("counter-incremented");
      set((state) => {
        state.count += state.step;
      });
    },
  })),
);
