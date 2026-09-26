import { createSlice } from "@reduxjs/toolkit";
import { fetchPlan } from "../api/subscriptions";
import { createAsyncThunk } from "./redux";

export const refreshPlan = createAsyncThunk(
  "session/refreshPlan",
  async (accountId: string) => (await fetchPlan(accountId)).plan,
  { serializeError: (error: unknown) => ({ message: "Could not refresh the plan" }) },
);

export const sessionSlice = createSlice({
  name: "session",
  initialState: { plan: null as string | null, loading: false, error: null as unknown },
  reducers: {},
  extraReducers: (builder) => {
    builder
      .addCase(refreshPlan.rejected, (state, action) => {
        state.loading = false;
        state.error = action.error;
      });
  },
});
