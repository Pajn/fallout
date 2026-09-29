const state = { items: [] as number[], count: 0 };

const Foo = (props: Record<string, unknown>) => <div>{Object.keys(props).length}</div>;

// Spreads the array's elements into a component's props.
export const Spread = () => <Foo key="items" {...state.items} />;

export const hasItems = () => state.items.length > 0;
