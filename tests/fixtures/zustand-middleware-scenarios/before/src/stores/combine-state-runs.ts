import { create } from "zustand";
import { combine } from "zustand/middleware";
import { track } from "../lib/state";

interface CounterState {
  count: number;
  step: number;
  openedAt?: number;
  increment: () => void;
}

export const TITLE = "Combine with state that runs";

export const useCounter = create(
  combine({ count: 0, step: 1, openedAt: track("counter-opened") }, (set) => ({
    increment: () => set((state) => ({ count: state.count + state.step })),
  })),
);
