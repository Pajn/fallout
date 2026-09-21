import { draw } from "../model/shape";

export default function DrawPage() {
  return <div>{draw({ sides: 4 })}</div>;
}
