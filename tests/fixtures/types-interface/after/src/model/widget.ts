export interface Widget {
  size: number;
  colour: string;
}

export const build = (size: number): Widget => ({ size });
