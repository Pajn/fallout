import { memo } from "./memo";
import { createAsyncThunk } from "./redux";

export const INVOICES = ["monthly", "yearly"];

export const refreshInvoices = createAsyncThunk(
  "billing/refreshInvoices",
  class {
    static {
      memo(2);
    }
  },
);
