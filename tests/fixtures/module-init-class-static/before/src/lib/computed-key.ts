import { key } from "./registry";

export const sibling = 1;

export class Keyed {
  [key(1)]() {
    return 1;
  }
}
