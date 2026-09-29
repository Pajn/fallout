import { create } from "zustand";
import { immer } from "zustand/middleware/immer";
import { logged } from "../lib/state";

interface CounterState {
  count: number;
  step: number;
  increment: () => void;
}

export const TITLE = "Own middleware";

export const useCounter = create<CounterState>()(
  immer(
    logged((set) => ({
      count: 0,
      step: 2,
      increment: () =>
        set((state) => {
          state.count += state.step;
        }),
    })),
  ),
);
