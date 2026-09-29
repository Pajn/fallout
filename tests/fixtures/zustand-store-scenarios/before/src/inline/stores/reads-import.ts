import { create } from "zustand";
import { DEFAULT_STEP } from "../config";

interface CounterState {
  count: number;
  step: number;
  increment: () => void;
}

export const TITLE = "Reads import";

export const useCounter = create<CounterState>((set) => ({
  count: 0,
  step: DEFAULT_STEP,
  increment: () => set((state) => ({ count: state.count + state.step })),
}));
