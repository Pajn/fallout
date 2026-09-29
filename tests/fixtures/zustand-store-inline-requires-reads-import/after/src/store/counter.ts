import { create } from "zustand";
import { DEFAULT_STEP } from "../config/counter";

interface CounterState {
  count: number;
  step: number;
  increment: () => void;
}

export const COUNTER_TITLE = "Counter";

export const useCounter = create<CounterState>((set) => ({
  count: 0,
  step: DEFAULT_STEP,
  increment: () => set((state) => ({ count: state.count + state.step })),
}));
