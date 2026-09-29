const state = { items: [] as number[], count: 0 };

const Foo = ({ children }: { children?: unknown }) => <div>{String(children)}</div>;

// Hands the array to a component whenever `flag` is set, and the component is
// free to change it.
export const ItemsView = ({ flag }: { flag: boolean }) => <Foo>{flag && state.items}</Foo>;

// Renders the count where it stands whenever `flag` is set.
export const CountView = ({ flag }: { flag: boolean }) => <li>{flag && state.count}</li>;

export const hasItems = () => state.items.length > 0;
export const hasCount = () => state.count > 0;
