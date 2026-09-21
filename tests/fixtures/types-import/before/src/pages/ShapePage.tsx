import type { Shape } from "../model/shape";

const square: Shape = { sides: 4 };

export default function ShapePage() {
  return <div>{square.sides}</div>;
}
