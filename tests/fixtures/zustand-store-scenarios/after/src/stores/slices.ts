import { create, type StateCreator } from "zustand";

interface CountSlice {
  count: number;
  increment: () => void;
}

interface StepSlice {
  step: number;
  setStep: (step: number) => void;
}

type CounterState = CountSlice & StepSlice;

const createCountSlice: StateCreator<CounterState, [], [], CountSlice> = (set) => ({
  count: 0,
  increment: () => set((state) => ({ count: state.count + state.step })),
});

const createStepSlice: StateCreator<CounterState, [], [], StepSlice> = (set) => ({
  step: 2,
  setStep: (step) => set({ step }),
});

export const TITLE = "Slices";

export const useCounter = create<CounterState>()((...a) => ({
  ...createCountSlice(...a),
  ...createStepSlice(...a),
}));
