import { create } from "zustand";
import { readSavedCount } from "../lib/state";

interface CounterState {
  count: number;
  step: number;
  increment: () => void;
}

export const TITLE = "Creator runs";

export const useCounter = create<CounterState>((set) => ({
  count: readSavedCount(),
  step: 1,
  increment: () => set((state) => ({ count: state.count + state.step })),
}));
