import { create } from "zustand";
import { create as createOwn } from "../lib/create";

interface CounterState {
  count: number;
  inc: () => void;
}

const counter = (set: (next: Partial<CounterState>) => void): CounterState => ({
  count: 0,
  inc: () => set({ count: 1 }),
});

// Only read, with `getState` and `subscribe`.
export const usePeek = create<CounterState>()(counter);
export const peekCount = () => `${usePeek.getState().count}`;
export const peekTotal = () => usePeek.getState().count + 2;

export const useWatch = create<CounterState>()(counter);
export const watchTitle = () =>
  useWatch.subscribe((state) => {
    document.title = `Count: ${state.count}`;
  });

// Written, through an action on what `getState` returns.
export const useBump = create<CounterState>()(counter);
export const bump = () => {
  useBump.getState().inc();
};

// Written, by handing out an action that a caller then calls.
export const useTick = create<CounterState>()(counter);
export const pickAction = () => useTick.getState().inc;
export const tick = () => pickAction()();

// Written, by a listener that sets the store.
export const useSync = create<CounterState>()(counter);
export const sync = () => useSync.subscribe(() => useSync.setState({ count: 1 }));

// Not Zustand's: the app's own `create`, and a class of the app.
export const useOwn = createOwn(() => ({ count: 0 }));
export const peekOwn = () => useOwn.getState().count + 0;

class Box {
  count = 0;
  getState() {
    this.count += 1;
    return this;
  }
}
export const box = new Box();
export const peekBox = () => box.getState().count + 0;
