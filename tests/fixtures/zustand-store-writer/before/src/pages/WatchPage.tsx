import { useWatch } from "../stores/reads";
export const WatchPage = () => <span>{useWatch((state) => state.count)}</span>;
