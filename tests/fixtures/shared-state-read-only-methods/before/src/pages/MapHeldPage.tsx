import { holds } from "../lib/writes";
export const MapHeldPage = () => <main className={holds("home", "yes") ? "held" : "empty"} />;
