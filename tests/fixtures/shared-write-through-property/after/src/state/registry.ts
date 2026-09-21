const registry = new Map();
export const register = (k) => { registry.set(k, 2); };
export const lookup = (k) => registry.get(k);
