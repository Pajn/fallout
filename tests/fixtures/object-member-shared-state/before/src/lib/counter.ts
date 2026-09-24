let count = 0;

export const counter = {
  bump: () => {
    count += 1;
  },
  read: () => count,
};
