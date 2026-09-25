const iso = { formatDate: (date: Date) => date.toISOString() };
export const local = { formatDate: (date: Date) => date.toLocaleDateString() };

export default iso;
