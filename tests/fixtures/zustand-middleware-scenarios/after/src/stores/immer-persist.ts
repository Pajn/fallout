import { create } from "zustand";
import { persist } from "zustand/middleware";
import { immer } from "zustand/middleware/immer";

interface CounterState {
  count: number;
  step: number;
  increment: () => void;
}

export const TITLE = "Immer around persist";

export const useCounter = create<CounterState>()(
  immer(
    persist(
      (set) => ({
        count: 0,
        step: 2,
        increment: () =>
        set((state) => {
          state.count += state.step;
        }),
      }),
      { name: "counter" },
    ),
  ),
);
