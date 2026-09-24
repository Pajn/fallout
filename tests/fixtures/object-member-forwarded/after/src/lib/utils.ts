const iso = { formatDate: (date: Date) => date.toISOString() };
const local = { formatDate: (date: Date) => date.toLocaleDateString() };

export { local as helpers, iso };
