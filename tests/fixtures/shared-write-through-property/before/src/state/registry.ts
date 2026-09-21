const registry = new Map();
export const register = (k) => { registry.set(k, 1); };
export const lookup = (k) => registry.get(k);
