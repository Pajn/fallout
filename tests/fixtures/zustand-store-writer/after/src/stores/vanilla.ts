import { createStore } from "zustand/vanilla";

interface CounterState {
  count: number;
}

export const store = createStore<CounterState>(() => ({ count: 0 }));

export const reset = () => store.setState({ count: 1 });
