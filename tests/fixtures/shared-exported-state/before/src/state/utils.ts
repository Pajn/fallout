export const utils = {
  items: [] as string[],
  format: (text: string) => text.toUpperCase(),
};

export function remember(item: string) {
  utils.items.push(item);
}
