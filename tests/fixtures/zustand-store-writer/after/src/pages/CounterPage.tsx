import { useCounter } from "../stores/counter";
export const CounterPage = () => <span>{useCounter((state) => state.count)}</span>;
