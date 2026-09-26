export function createAsyncThunk<Arg, Result>(type: string, run: (arg: Arg) => Promise<Result>) {
  const thunk = (arg: Arg) => run(arg);
  return Object.assign(thunk, {
    pending: { type: `${type}/pending` },
    fulfilled: { type: `${type}/fulfilled` },
    rejected: { type: `${type}/rejected` },
  });
}
