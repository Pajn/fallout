const iso = { formatDate: (date: Date) => date.toISOString() };
const local = { formatDate: (date: Date) => date.toLocaleDateString() };

export { iso as helpers, local };
