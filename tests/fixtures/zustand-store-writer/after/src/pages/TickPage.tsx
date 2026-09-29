import { useTick } from "../stores/reads";
export const TickPage = () => <span>{useTick((state) => state.count)}</span>;
