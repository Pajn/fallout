import { hasItems } from "../state/return-member";
export const ReturnMemberPage = () => <p>{hasItems() ? "Items" : "None"}</p>;
