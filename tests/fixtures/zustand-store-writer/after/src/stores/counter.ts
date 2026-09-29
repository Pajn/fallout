import { create } from "zustand";

interface CounterState {
  count: number;
}

export const TITLE = "Counter";

export const useCounter = create<CounterState>(() => ({ count: 0 }));

export const reset = () => useCounter.setState({ count: 1 });
