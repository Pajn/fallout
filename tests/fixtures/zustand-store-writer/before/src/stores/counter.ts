import { create } from "zustand";

interface CounterState {
  count: number;
}

export const TITLE = "Counter";

export const useCounter = create<CounterState>(() => ({ count: 0 }));

export const reset = () => useCounter.setState({ count: 0 });

// Reads the state, and writes nothing.
export const countNow = () => useCounter.getState().count * 2;
