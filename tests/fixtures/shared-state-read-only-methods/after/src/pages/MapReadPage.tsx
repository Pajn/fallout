import { isCached } from "../lib/reads";
export const MapReadPage = () => <main className={isCached("home") ? "cached" : "fresh"} />;
