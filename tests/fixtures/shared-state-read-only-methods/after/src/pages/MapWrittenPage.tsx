import { isCached } from "../lib/writes";
export const MapWrittenPage = () => <main className={isCached("home") ? "cached" : "fresh"} />;
