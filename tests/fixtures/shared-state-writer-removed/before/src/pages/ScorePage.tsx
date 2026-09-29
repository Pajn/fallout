import { readScore } from "../state/score";
export const ScorePage = () => <span>{readScore()}</span>;
