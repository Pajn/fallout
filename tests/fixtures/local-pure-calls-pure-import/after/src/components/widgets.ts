import { memo } from "./memo";
function make(label) { return memo({ label }); }
export const Panel = make("sidebar");
export const VERSION = "1.0";
