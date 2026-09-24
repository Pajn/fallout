import { utils } from "../lib/utils";
const { formatDate } = utils;
export const DatePage = () => formatDate(new Date());
