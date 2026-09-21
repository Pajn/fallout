export const LazyPage = async () => {
  const panel = await import("../panels/heavy");
  return panel.render();
};
