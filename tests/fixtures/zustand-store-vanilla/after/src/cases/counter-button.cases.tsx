import { useStore } from "zustand";
import { counterStore } from "../store/counter";

export const CounterButton = () => {
  const count = useStore(counterStore, (state) => state.count);
  return (
    <button type="button" onClick={() => counterStore.getState().increment()}>
      {count}
    </button>
  );
};
