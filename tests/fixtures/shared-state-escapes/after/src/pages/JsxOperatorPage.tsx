import { hasItems } from "../state/jsx-operator";
export const JsxOperatorPage = () => <p>{hasItems() ? "Items" : "None"}</p>;
