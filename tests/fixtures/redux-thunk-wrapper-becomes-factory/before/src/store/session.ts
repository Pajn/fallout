import { fetchPlan } from "../api/subscriptions";
import { createAsyncThunk } from "./redux";

export const refreshPlan = createAsyncThunk(
  "session/refreshPlan",
  async (accountId: string) => (await fetchPlan(accountId)).plan,
);

export const LABEL = "Plan";
