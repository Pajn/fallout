let count = 0;

export const status = () => {
  if (count) return "started";
  return "idle";
};

export const next = () => count + 1;

export const bump = () => {
  count = count + 2;
};

export const isAtLimit = () => count >= 10;
