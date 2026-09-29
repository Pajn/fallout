export const events: string[] = [];

export function track(name: string) {
  return events.push(name);
}
