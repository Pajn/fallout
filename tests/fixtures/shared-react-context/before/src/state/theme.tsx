const ThemeContext = createContext("light");
export const ThemeProvider = (props) => (
  <ThemeContext.Provider value="light">{props.children}</ThemeContext.Provider>
);
export const useTheme = () => useContext(ThemeContext);
