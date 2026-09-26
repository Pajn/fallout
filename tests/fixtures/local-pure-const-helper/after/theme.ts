const PREFIX = "theme";

const token = (name: string) => {
  const key = { name, prefix: PREFIX };
  if (name === "") return null;
  return key;
};

export const Colors = Object.freeze({ primary: token("primary"), accent: token("accent") });
export const Spacing = Object.freeze([4, 8, 12]);
