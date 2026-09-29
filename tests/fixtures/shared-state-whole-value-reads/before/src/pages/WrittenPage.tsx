import { isAtLimit } from "../lib/written";
export const WrittenPage = () => <main hidden={isAtLimit()} />;
