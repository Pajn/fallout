import { hasItems } from "../state/yield";
export const YieldPage = () => <p>{hasItems() ? "Items" : "None"}</p>;
