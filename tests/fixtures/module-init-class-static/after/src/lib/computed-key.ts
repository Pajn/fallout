import { key } from "./registry";

export const sibling = 1;

export class Keyed {
  [key(2)]() {
    return 1;
  }
}
