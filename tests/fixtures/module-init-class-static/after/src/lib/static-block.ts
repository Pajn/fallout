import { register } from "./registry";

export const sibling = 1;

export class Store {
  static {
    register(2);
  }
}
