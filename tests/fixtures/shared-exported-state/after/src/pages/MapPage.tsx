import { cache } from "../state/map";
export const MapPage = () => <span>{cache.get("a")}</span>;
