import { build } from "./builder";
function make(label) { return /* @__PURE__ */ build({ label }); }
export const Panel = make("sidebar");
export const VERSION = "1.0";
