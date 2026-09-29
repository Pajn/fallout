import { isAtLimit } from "../lib/tested";
export const TestedPage = () => <main hidden={isAtLimit()} />;
