import { create } from "zustand";
import { immer } from "zustand/middleware/immer";

interface CounterState {
  count: number;
  step: number;
  increment: () => void;
}

export const TITLE = "Immer";

export const useCounter = create<CounterState>()(
  immer((set) => ({
    count: 0,
    step: 2,
    increment: () =>
        set((state) => {
          state.count += state.step;
        }),
  })),
);
