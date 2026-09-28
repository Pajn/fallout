import { memo } from "./memo";

export const sibling = memo(1);

export const X = class {
  static {
    const memo = () => ((globalThis as any).f = 2);
    memo();
  }
};
