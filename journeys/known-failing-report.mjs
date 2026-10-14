function reportTests(suites) {
  return suites.flatMap((suite) => [
    ...(suite.specs ?? []).flatMap((spec) => (spec.tests ?? []).map((test) => ({
      tags: spec.tags ?? [],
      test,
    }))),
    ...reportTests(suite.suites ?? []),
  ]);
}

export function expectedKnownFailures(report, count) {
  if ((report.errors?.length ?? 0) > 0) return false;
  const tests = reportTests(report.suites ?? []);
  if (tests.length !== count) return false;
  return tests.every(({ tags, test }) => {
    const issueNumbers = tags.flatMap((tag) => {
      const match = /^@?known-failing-([1-9][0-9]*)$/.exec(tag);
      return match ? [match[1]] : [];
    });
    if (issueNumbers.length !== 1) return false;
    const marker = `[steward-known-failure:#${issueNumbers[0]}]`;
    const result = test.results?.at(-1);
    if (result?.status !== "failed" || !result.errors?.length) return false;
    return result.errors.every((error) => `${error.message ?? ""}\n${error.value ?? ""}\n${error.stack ?? ""}`.includes(marker));
  });
}
