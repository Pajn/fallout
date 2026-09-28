import { register } from "./registry";

export const sibling = 1;

export class Menu {
  open() {
    return register(2);
  }
}
