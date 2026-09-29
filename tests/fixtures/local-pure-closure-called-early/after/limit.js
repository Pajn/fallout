function makeReader() {
  return () => LIMIT;
}
const read = makeReader();
export const limit = read();
export const label = "limit";
const LIMIT = 20;
