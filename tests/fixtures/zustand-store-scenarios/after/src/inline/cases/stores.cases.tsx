import { useCounter as usePlain } from "../stores/plain";
import { useCounter as useReadsImport } from "../stores/reads-import";

export const Counts = () => (
  <ul>
    <li>{usePlain((state) => state.count)}</li>
    <li>{useReadsImport((state) => state.count)}</li>
  </ul>
);
