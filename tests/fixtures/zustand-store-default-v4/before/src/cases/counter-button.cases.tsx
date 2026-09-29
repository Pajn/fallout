import { useCounter } from "../store/counter";

export const CounterButton = () => {
  const count = useCounter((state) => state.count);
  const increment = useCounter((state) => state.increment);
  return (
    <button type="button" onClick={increment}>
      {count}
    </button>
  );
};
