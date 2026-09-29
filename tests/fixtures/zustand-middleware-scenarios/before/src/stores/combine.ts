import { create } from "zustand";
import { combine } from "zustand/middleware";

interface CounterState {
  count: number;
  step: number;
  increment: () => void;
}

export const TITLE = "Combine";

export const useCounter = create(
  combine({ count: 0, step: 1 }, (set) => ({
    increment: () => set((state) => ({ count: state.count + state.step })),
  })),
);
