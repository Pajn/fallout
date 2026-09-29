import { create } from "zustand";
import { persist } from "zustand/middleware";

interface CounterState {
  count: number;
  step: number;
  increment: () => void;
}

export const TITLE = "Persist";

export const useCounter = create<CounterState>()(
  persist(
    (set) => ({
      count: 0,
      step: 1,
      increment: () => set((state) => ({ count: state.count + state.step })),
    }),
    { name: "counter" },
  ),
);
