import { makeQueue } from "./make";

const queue = makeQueue();
export function enqueue() { queue.pending.items.push(1); }
export const waiting = () => queue.pending.items.length;
export const heading = () => "Queue";
