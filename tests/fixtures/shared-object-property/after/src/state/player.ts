const state = { theme: "light", volume: 1 };

export const setTheme = (theme: string) => {
  state.theme = theme.trim();
};

export const setVolume = (volume: number) => {
  state.volume = volume;
};

export const volume = () => state.volume;
