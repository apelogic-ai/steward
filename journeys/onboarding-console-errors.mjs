const onboardingRequestPath = /^\/app\/api\/v1\/runs\/[^/]+\/github\//;

export function onlyOnboardingRequestUnavailable(errors, pageUrl) {
  if (errors.length === 0) return false;
  const pageOrigin = new URL(pageUrl).origin;
  return errors.every(({ text, url }) => {
    if (!url || !/\b503\b/.test(text)) return false;
    try {
      const source = new URL(url);
      return source.origin === pageOrigin && onboardingRequestPath.test(source.pathname);
    } catch {
      return false;
    }
  });
}

export function onlyZeroBudgetTaskSaveRejected(errors, responses, pageUrl) {
  if (errors.length !== 1 || responses.length !== 1) return false;
  const [{ text, url }] = errors;
  const [response] = responses;
  if (!url || !/\b422\b/.test(text)) return false;
  try {
    const pageOrigin = new URL(pageUrl).origin;
    const source = new URL(url);
    const responseSource = new URL(response.url);
    const exactTaskPath = (candidate) => candidate.origin === pageOrigin
      && candidate.pathname === "/app/api/v1/tasks"
      && candidate.search === "";
    return exactTaskPath(source)
      && exactTaskPath(responseSource)
      && response.method === "POST"
      && response.status === 422;
  } catch {
    return false;
  }
}
