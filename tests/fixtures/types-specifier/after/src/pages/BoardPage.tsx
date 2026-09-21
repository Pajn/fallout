import { render } from "../model/board";

export default function BoardPage() {
  return <div>{render({ sides: 4 })}</div>;
}
