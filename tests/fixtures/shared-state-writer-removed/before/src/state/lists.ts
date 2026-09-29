export const lists = {
  items: [] as string[],
  tags: [] as string[],
};

export function remember(item: string) {
  lists.items.push(item);
}
