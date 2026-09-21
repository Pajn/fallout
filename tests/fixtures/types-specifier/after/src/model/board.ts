import { draw, type Shape as Form } from "./shape";

interface Corner {
  x: number;
}

export { type Corner as Edge };

export const render = (shape) => draw(shape);
