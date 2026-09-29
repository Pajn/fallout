import { hasItems } from "../state/await";
export const AwaitPage = () => <p>{hasItems() ? "Items" : "None"}</p>;
