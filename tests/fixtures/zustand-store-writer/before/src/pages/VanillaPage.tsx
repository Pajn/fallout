import { useStore } from "zustand";
import { store } from "../stores/vanilla";
export const VanillaPage = () => <span>{useStore(store, (state) => state.count)}</span>;
