import { inQueue } from "../lib/escapes";
export const QueuePage = () => <main className={inQueue("home") ? "queued" : "idle"} />;
