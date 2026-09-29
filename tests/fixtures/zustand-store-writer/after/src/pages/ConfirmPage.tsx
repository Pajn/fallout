import { useConfirm } from "../stores/confirm";
export const ConfirmPage = () => <span>{useConfirm((state) => state.count)}</span>;
