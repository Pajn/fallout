import { hasId } from "../lib/escapes";
export const FindPage = () => <main className={hasId("home") ? "found" : "missing"} />;
