import { hasItem } from "../lib/reads";
export const ListReadPage = () => <main className={hasItem("home") ? "listed" : "new"} />;
