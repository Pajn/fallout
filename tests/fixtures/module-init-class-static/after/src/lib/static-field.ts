import { register } from "./registry";

export const sibling = 1;

export class Widget {
  static id = register(2);
}
