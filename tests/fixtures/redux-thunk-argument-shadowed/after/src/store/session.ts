import { memo } from "./memo";
import { createAsyncThunk } from "./redux";

export const PLANS = ["free", "premium"];

export const refreshPlan = createAsyncThunk(
  "session/refreshPlan",
  class {
    static {
      const memo = () => ((globalThis as any).refreshed = 2);
      memo();
    }
  },
);
