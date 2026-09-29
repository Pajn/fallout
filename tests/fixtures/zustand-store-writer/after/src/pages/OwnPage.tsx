import { useOwn } from "../stores/reads";
export const OwnPage = () => <span>{useOwn.getState().count}</span>;
