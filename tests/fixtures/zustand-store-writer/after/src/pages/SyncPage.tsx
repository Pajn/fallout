import { useSync } from "../stores/reads";
export const SyncPage = () => <span>{useSync((state) => state.count)}</span>;
