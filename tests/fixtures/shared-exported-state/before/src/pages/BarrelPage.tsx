import { cache } from "../state/barrel";
export const BarrelPage = () => <span>{cache.get("a")}</span>;
