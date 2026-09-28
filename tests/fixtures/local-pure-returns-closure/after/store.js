function createCounter() {
  return {
    count: 0,
    inc: () => 2,
    reset() { return 0; },
  };
}
export const counter = createCounter();
export const label = "counter";
