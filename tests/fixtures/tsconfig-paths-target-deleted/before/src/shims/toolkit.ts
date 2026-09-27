// Warms each thunk's cache while its module is evaluated.
export function createAsyncThunk<T>(type: string, run: (arg: string) => Promise<T>) {
  void run("warm");
  return Object.assign((arg: string) => run(arg), { typePrefix: type });
}

export function createSlice<S>(options: { name: string; initialState: S }) {
  return { name: options.name, reducer: (state: S = options.initialState) => state };
}
