import { useTheme } from "../state/theme";
export const ThemedPage = () => <span>{useTheme()}</span>;
