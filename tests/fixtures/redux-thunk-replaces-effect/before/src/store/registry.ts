const registered: string[] = [];
(globalThis as { thunks?: string[] }).thunks = registered;

export function register(type: string) {
  registered.push(type);
  return { type };
}
