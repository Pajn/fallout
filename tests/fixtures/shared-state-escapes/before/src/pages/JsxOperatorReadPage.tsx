import { hasCount } from "../state/jsx-operator";
export const JsxOperatorReadPage = () => <p>{hasCount() ? "Counted" : "None"}</p>;
