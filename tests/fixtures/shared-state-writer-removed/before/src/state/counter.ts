let count = 0;

export function bump() {
  count += 1;
}

export const read = () => count;
