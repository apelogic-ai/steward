export function browserInputsRequired(tags) {
  return !["@callers", "@callers and not @live-jira-webhook"].includes(tags.trim());
}
