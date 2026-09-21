import { build } from "../model/widget";
import type { Widget } from "../model/widget";

export default function WidgetPage() {
  const widget: Widget = build(3);
  return <div>{widget.size}</div>;
}
