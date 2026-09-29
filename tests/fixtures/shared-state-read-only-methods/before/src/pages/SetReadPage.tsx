import { wasSeen } from "../lib/reads";
export const SetReadPage = () => <main className={wasSeen("home") ? "seen" : "new"} />;
