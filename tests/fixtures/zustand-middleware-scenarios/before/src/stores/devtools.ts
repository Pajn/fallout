import { create } from "zustand";
import { devtools } from "zustand/middleware";

interface CounterState {
  count: number;
  step: number;
  increment: () => void;
}

export const TITLE = "Devtools";

export const useCounter = create<CounterState>()(
  devtools(
    (set) => ({
      count: 0,
      step: 1,
      increment: () => set((state) => ({ count: state.count + state.step })),
    }),
    { name: "counter" },
  ),
);
