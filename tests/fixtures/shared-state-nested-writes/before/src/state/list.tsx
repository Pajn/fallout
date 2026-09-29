const makeList = () => ({ items: [] as number[] });
let list = makeList();
export function clear() { list = makeList(); }

const List = ({ items }: { items: number[] }) => (
  <ul>
    {items.map((item) => (
      <li key={item}>{item}</li>
    ))}
  </ul>
);

// Hands the array to a component, which is free to change it.
export const ListView = () => <List items={list.items} />;
export const size = () => list.items.length;
