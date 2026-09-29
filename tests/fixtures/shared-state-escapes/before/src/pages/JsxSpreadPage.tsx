import { hasItems } from "../state/jsx-spread";
export const JsxSpreadPage = () => <p>{hasItems() ? "Items" : "None"}</p>;
