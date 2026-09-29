import { hasItems } from "../state/literal-returned";
export const LiteralReturnedPage = () => <p>{hasItems() ? "Items" : "None"}</p>;
