const which = "./fallback";
const loaded = require(which);
const unrelated = () => 1;
export const helper = () => loaded;
