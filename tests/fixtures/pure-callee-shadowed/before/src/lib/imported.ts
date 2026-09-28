import { memo } from "./memo";

export const sibling = memo(1);

export const Y = class {
  static {
    memo(1);
  }
};
