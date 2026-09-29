import { isAtLimit } from "../lib/added";
export const AddedPage = () => <main hidden={isAtLimit()} />;
