export async function fetchPlan(accountId: string) {
  const response = await fetch(`/accounts/${accountId}/plan`);
  return (await response.json()) as { plan: string };
}
