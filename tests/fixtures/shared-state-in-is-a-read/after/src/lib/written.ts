let prefs: Record<string, string> = { theme: "light" };

export const isSet = (key: string) => key in prefs;

export const isCurrent = (other?: Record<string, string>) => prefs === other;

export const set = (key: string, value: string) => {
  prefs = { ...prefs, [key]: value.trim() };
};

export const theme = () => prefs.theme;
