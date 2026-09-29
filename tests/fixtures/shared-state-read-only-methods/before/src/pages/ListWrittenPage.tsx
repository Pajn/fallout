import { findsItem } from "../lib/writes";
export const ListWrittenPage = () => <main className={findsItem("home") ? "listed" : "new"} />;
