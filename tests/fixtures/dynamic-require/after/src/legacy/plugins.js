const which = "./fallback";
const loaded = require(which);
const unrelated = () => 99;
export const helper = () => loaded;
