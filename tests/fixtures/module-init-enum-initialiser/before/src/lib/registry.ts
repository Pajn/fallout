export const register = (id: number) => {
  (globalThis as any).registered = id;
  return id;
};
