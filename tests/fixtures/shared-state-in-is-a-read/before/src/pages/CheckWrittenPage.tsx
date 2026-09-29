import { isSet } from "../lib/written";
export const CheckWrittenPage = () => <main hidden={!isSet("theme")} />;
