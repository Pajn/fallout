import { isCached } from "../lib/escapes";
export const LookupPage = () => <main className={isCached("home") ? "cached" : "fresh"} />;
