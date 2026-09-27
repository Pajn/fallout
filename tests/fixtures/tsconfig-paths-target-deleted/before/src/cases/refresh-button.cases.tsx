import { refreshPlan } from "../store/session";

export const RefreshButton = (props: { dispatch: (action: unknown) => void }) => (
  <button type="button" onClick={() => props.dispatch(refreshPlan("account"))}>
    Refresh
  </button>
);
