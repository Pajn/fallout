const ThemeContext = createContext("light");
export const ThemeProvider = (props) => (
  <ThemeContext.Provider value="dark">{props.children}</ThemeContext.Provider>
);
export const useTheme = () => useContext(ThemeContext);
