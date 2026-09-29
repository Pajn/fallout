import { hasItems } from "../state/export-default";
export const ExportDefaultPage = () => <p>{hasItems() ? "Items" : "None"}</p>;
