import { createAsyncThunk as createAsyncThunkBase } from "@reduxjs/toolkit";

export interface ThunkApi {
  state: { session: { plan: string | null } };
}

export const createAsyncThunk = createAsyncThunkBase.withTypes<ThunkApi>();
