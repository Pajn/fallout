export const register = (id: unknown) => {
  (globalThis as any).registered = id;
  return id;
};

export const mixin = (n: number) =>
  class {
    n = n;
  };

export const key = (n: number) => `key${n}`;
