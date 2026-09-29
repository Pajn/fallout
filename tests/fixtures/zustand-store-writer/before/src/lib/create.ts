// The app's own `create`, which is not Zustand's, whatever its methods are called.
export function create<T>(initial: () => T) {
  let state = initial();
  return {
    getState: () => state,
    setState: (next: T) => {
      state = next;
    },
  };
}
