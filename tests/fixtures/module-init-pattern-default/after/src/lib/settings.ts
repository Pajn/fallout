import { register } from "./registry";

const defaults: { id?: number } = {};

export const sibling = 1;

export const { id = register(2) } = defaults;
