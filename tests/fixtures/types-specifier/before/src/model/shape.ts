export interface Shape {
  sides: number;
}

export const draw = (shape: Shape): string => `${shape.sides}`;
