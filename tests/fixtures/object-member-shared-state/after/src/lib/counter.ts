let count = 0;

export const counter = {
  bump: () => {
    count += 2;
  },
  read: () => count,
};
