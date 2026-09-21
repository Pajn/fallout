import * as api from "../api";
export const UserPage = () => register(api) && api.fetchUser();
