import { hasLabel } from "../lib/escapes";
export const ShoutPage = () => <main className={hasLabel("HOME") ? "loud" : "quiet"} />;
