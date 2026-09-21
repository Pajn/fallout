import { type Ready, ready } from "./ready";

export interface Gadget {
  size: number;
  colour: string;
}

export class Store implements Ready {
  loaded!: boolean;
  count?: number;
}

export const make = (): {
  store: Store;
  ready: boolean;
} => ({ store: new Store(), ready: ready() });
