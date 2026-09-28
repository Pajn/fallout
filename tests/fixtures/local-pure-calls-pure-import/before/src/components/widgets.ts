import { memo } from "./memo";
function make(label) { return memo({ label }); }
export const Panel = make("panel");
export const VERSION = "1.0";
