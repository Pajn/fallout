import { useCounter as useImmer } from "../stores/immer";
import { useCounter as useSubscribeWithSelector } from "../stores/subscribe-with-selector";
import { useCounter as useCombine } from "../stores/combine";
import { useCounter as useNested } from "../stores/nested";
import { useCounter as useDevtools } from "../stores/devtools";
import { useCounter as useImmerPersist } from "../stores/immer-persist";
import { useCounter as useOwnMiddleware } from "../stores/own-middleware";
import { useCounter as useCombineStateRuns } from "../stores/combine-state-runs";
import { useCounter as useImmerCreatorRuns } from "../stores/immer-creator-runs";
import { useCounter as useImmerEffectRemoved } from "../stores/immer-effect-removed";
import { useCounter as useImmerEffectAdded } from "../stores/immer-effect-added";
import { useCounter as useInlineImmer } from "../inline/stores/immer";

export const Counts = () => (
  <ul>
    <li>{useImmer((state) => state.count)}</li>
    <li>{useSubscribeWithSelector((state) => state.count)}</li>
    <li>{useCombine((state) => state.count)}</li>
    <li>{useNested((state) => state.count)}</li>
    <li>{useDevtools((state) => state.count)}</li>
    <li>{useImmerPersist((state) => state.count)}</li>
    <li>{useOwnMiddleware((state) => state.count)}</li>
    <li>{useCombineStateRuns((state) => state.count)}</li>
    <li>{useImmerCreatorRuns((state) => state.count)}</li>
    <li>{useImmerEffectRemoved((state) => state.count)}</li>
    <li>{useImmerEffectAdded((state) => state.count)}</li>
    <li>{useInlineImmer((state) => state.count)}</li>
  </ul>
);
