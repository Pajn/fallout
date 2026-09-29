import { wasSeen } from "../lib/writes";
export const SetWrittenPage = () => <main className={wasSeen("home") ? "seen" : "new"} />;
