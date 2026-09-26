import { createSlice } from "@reduxjs/toolkit";
import { fetchPlan } from "../api/subscriptions";
import { createAsyncThunk } from "./index";

export const refreshPlan = createAsyncThunk(
  "session/refreshPlan",
  async (accountId: string) => (await fetchPlan(accountId.trim())).plan,
);

export const sessionSlice = createSlice({
  name: "session",
  initialState: { plan: null as string | null, loading: false },
  reducers: {
    cleared: (state) => {
      state.plan = null;
    },
  },
  extraReducers: (builder) => {
    builder
      .addCase(refreshPlan.pending, (state) => {
        state.loading = true;
      })
      .addCase(refreshPlan.fulfilled, (state, action) => {
        state.loading = false;
        state.plan = action.payload;
      })
      .addCase(refreshPlan.rejected, (state) => {
        state.loading = false;
      });
  },
});
