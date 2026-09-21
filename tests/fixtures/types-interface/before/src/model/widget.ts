export interface Widget {
  size: number;
}

export const build = (size: number): Widget => ({ size });
