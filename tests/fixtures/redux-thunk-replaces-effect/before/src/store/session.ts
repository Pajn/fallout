import { fetchPlan } from "../api/subscriptions";
import { createAsyncThunk } from "./redux";
import { register } from "./registry";

export const refreshPlan = register("session/refreshPlan");
export const loadPlan = fetchPlan;
export const LABEL = "Plan";
