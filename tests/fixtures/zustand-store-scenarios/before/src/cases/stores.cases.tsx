import { useStore } from "zustand";
import { useCounter as useAlias } from "../stores/alias";
import { useCounter as useCreatorRuns } from "../stores/creator-runs";
import { useCounter as useCurried } from "../stores/curried";
import { useCounter as useCustomCreate } from "../stores/custom-create";
import { useCounter as useDefaultV4 } from "../stores/default-v4";
import { useCounter as useEffectRemoved } from "../stores/effect-removed";
import { useCounter as useNamespace } from "../stores/namespace";
import { useCounter as usePersist } from "../stores/persist";
import { useCounter as usePlain } from "../stores/plain";
import { useCounter as useReactSubpath } from "../stores/react-subpath";
import { useCounter as useReadBeforeDeclaration } from "../stores/read-before-declaration";
import { useCounter as useReexport } from "../stores/reexport";
import { useCounter as useSlices } from "../stores/slices";
import { counterStore } from "../stores/vanilla";
import { useCounter as useWithTypes } from "../stores/with-types-not-identity";
import { useCounter as useShim } from "../shim/store";

export const Counts = () => (
  <ul>
    <li>{usePlain((state) => state.count)}</li>
    <li>{useCurried((state) => state.count)}</li>
    <li>{useNamespace((state) => state.count)}</li>
    <li>{useAlias((state) => state.count)}</li>
    <li>{useStore(counterStore, (state) => state.count)}</li>
    <li>{useDefaultV4((state) => state.count)}</li>
    <li>{useReactSubpath((state) => state.count)}</li>
    <li>{useReexport((state) => state.count)}</li>
    <li>{useEffectRemoved((state) => state.count)}</li>
    <li>{useCustomCreate((state) => state.count)}</li>
    <li>{useCreatorRuns((state) => state.count)}</li>
    <li>{useReadBeforeDeclaration((state) => state.count)}</li>
    <li>{usePersist((state) => state.count)}</li>
    <li>{useSlices((state) => state.count)}</li>
    <li>{useWithTypes((state) => state.count)}</li>
    <li>{useShim().count}</li>
  </ul>
);
