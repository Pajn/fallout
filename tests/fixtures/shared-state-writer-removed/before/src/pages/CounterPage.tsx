import { read } from "../state/counter";
export const CounterPage = () => <span>{read()}</span>;
