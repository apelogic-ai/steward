export function browserInputsRequired(tags) {
  return ![
    "@callers",
    "@callers and not @live-jira-webhook",
    "@callers and not @covered-by-process-e2e",
    "@callers and not @live-jira-webhook and not @covered-by-process-e2e",
  ].includes(tags.trim());
}
