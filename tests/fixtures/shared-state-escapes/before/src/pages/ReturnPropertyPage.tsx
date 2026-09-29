import { hasItems } from "../state/return-property";
export const ReturnPropertyPage = () => <p>{hasItems() ? "Items" : "None"}</p>;
