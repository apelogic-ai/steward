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
