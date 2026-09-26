import { createAsyncThunk as createAsyncThunkBase } from "@reduxjs/toolkit";

export const createAsyncThunk = <T,>(type: string, run: (arg: string) => Promise<T>) => {
  (globalThis as { lastThunk?: string }).lastThunk = type;
  return createAsyncThunkBase(type, run);
};
