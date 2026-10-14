export function defaultWorkspaceName(repositoryName) {
  let normalized = "";
  let previousDash = false;
  for (const byte of new TextEncoder().encode(repositoryName)) {
    const alphaNumeric = byte >= 48 && byte <= 57 || byte >= 65 && byte <= 90 || byte >= 97 && byte <= 122;
    const next = alphaNumeric ? String.fromCharCode(byte).toLocaleLowerCase("en-US") : "-";
    if (next === "-") {
      if (!normalized || previousDash) continue;
      previousDash = true;
    } else {
      previousDash = false;
    }
    if (normalized.length === 40) break;
    normalized += next;
  }
  return normalized.replace(/-+$/, "");
}
