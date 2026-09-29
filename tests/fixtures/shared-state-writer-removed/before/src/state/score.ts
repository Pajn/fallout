let score = 0;

export function win() {
  score += 1;
}

export const readScore = () => score;

export const LABEL = "Score";

export const hasScore = () => score > 0;

export const UNIT = "points";
