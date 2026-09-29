import * as s from "../state/namespace";
export const NamespacePage = () => <span>{s.cache.get("a")}</span>;
