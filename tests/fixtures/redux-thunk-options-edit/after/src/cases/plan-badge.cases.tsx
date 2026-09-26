import { sessionSlice } from "../store/session";

export const cases = {
  reducers: { session: sessionSlice.reducer },
  preloadedState: { session: { plan: "premium", loading: false } },
};
