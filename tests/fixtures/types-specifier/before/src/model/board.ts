import { draw, type Shape } from "./shape";

interface Corner {
  x: number;
}

export { type Corner };

export const render = (shape) => draw(shape);
