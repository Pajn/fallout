export function readSavedCount() {
  return Number(localStorage.getItem("counter") ?? 0);
}
