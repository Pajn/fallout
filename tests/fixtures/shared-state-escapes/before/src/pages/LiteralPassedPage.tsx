import { hasItems } from "../state/literal-passed";
export const LiteralPassedPage = () => <p>{hasItems() ? "Items" : "None"}</p>;
