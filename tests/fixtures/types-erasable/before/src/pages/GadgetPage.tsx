import { make } from "../model/gadget";

export default function GadgetPage() {
  return <div>{String(make().ready)}</div>;
}
