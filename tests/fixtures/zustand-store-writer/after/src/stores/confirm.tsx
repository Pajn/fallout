import { create } from "zustand";

interface CounterState {
  count: number;
  inc: () => void;
}

const Confirm = ({ label, children }: { label?: string; children: () => void }) => (
  <button onClick={children}>{label ?? "OK"}</button>
);

// Written, by handing an action to a component as its child, which calls it.
export const useConfirm = create<CounterState>()((set) => ({
  count: 0,
  inc: () => set({ count: 1 }),
}));
export const ConfirmButton = () => <Confirm label="Add one">{useConfirm.getState().inc}</Confirm>;
