export async function fetchPlan(accountId: string) {
  const response = await fetch(`/v2/accounts/${accountId}/plan`);
  return (await response.json()) as { plan: string };
}
