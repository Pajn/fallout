import { useBump } from "../stores/reads";
export const BumpPage = () => <span>{useBump((state) => state.count)}</span>;
