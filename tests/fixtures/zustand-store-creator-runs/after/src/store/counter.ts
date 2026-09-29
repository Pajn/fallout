import { create } from "zustand";
import { readSavedCount } from "../lib/storage";

interface CounterState {
  count: number;
  step: number;
  increment: () => void;
}

export const COUNTER_TITLE = "Counter";

export const useCounter = create<CounterState>((set) => ({
  count: readSavedCount(),
  step: 2,
  increment: () => set((state) => ({ count: state.count + state.step })),
}));
