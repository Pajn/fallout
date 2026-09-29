import { create } from "zustand";
import { subscribeWithSelector } from "zustand/middleware";

interface CounterState {
  count: number;
  step: number;
  increment: () => void;
}

export const TITLE = "Subscribe with selector";

export const useCounter = create<CounterState>()(
  subscribeWithSelector((set) => ({
    count: 0,
    step: 2,
    increment: () => set((state) => ({ count: state.count + state.step })),
  })),
);
