import { hasItems } from "../state/member-alias";
export const MemberAliasPage = () => <p>{hasItems() ? "Items" : "None"}</p>;
