import { create } from "zustand";
import { combine, subscribeWithSelector } from "zustand/middleware";
import { immer } from "zustand/middleware/immer";

interface CounterState {
  count: number;
  step: number;
  increment: () => void;
}

export const TITLE = "Nested";

export const useCounter = create<CounterState>()(
  immer(
    subscribeWithSelector(
      combine({ count: 0, step: 1 }, (set) => ({
        increment: () => set((state) => ({ count: state.count + state.step })),
      })),
    ),
  ),
);
