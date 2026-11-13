import type { BrowserEnvelope, EgressAuthority, EgressHost } from "@/api-client";
import type { TagSelectOption } from "@/components/hs";

/** The `host:port` key the browser uses for one web access host. */
export function webAccessHostKey(host: EgressHost): string {
  return `${host.host}:${host.port ?? 443}`;
}

function hostKeys(hosts: ReadonlyArray<EgressHost> | null | undefined): Set<string> {
  return new Set((hosts ?? []).map(webAccessHostKey));
}

/** The template's web access hosts that this installation still allows, in template order. */
export function allowedWebAccessHosts(templateHosts: ReadonlyArray<EgressHost> | null | undefined, installationHosts: ReadonlyArray<EgressHost> | null | undefined): Array<EgressHost> {
  const allowed = hostKeys(installationHosts);
  return (templateHosts ?? []).filter((host) => allowed.has(webAccessHostKey(host)));
}

/** The chosen hosts as an Envelope grant, or nothing when none are chosen. */
export function webAccessGrant(hosts: ReadonlyArray<EgressHost>, selected: ReadonlySet<string>): EgressAuthority | undefined {
  const chosen = hosts.filter((host) => selected.has(webAccessHostKey(host)));
  return chosen.length ? { hosts: chosen } : undefined;
}

/** The Envelope an administrator provisions from a template: the template, with only the chosen web access hosts. */
export function provisionedEnvelope(template: BrowserEnvelope, selected: ReadonlySet<string>): BrowserEnvelope {
  const { egress: templateEgress, ...spec } = template.spec;
  const egress = webAccessGrant(templateEgress?.hosts ?? [], selected);
  return { ...template, spec: egress ? { ...spec, egress } : spec };
}

/** Template editor options: every host this installation allows, plus selected hosts it no longer allows, which can only be removed. */
export function templateWebAccessOptions(installationHosts: ReadonlyArray<EgressHost> | null | undefined, selectedHosts: ReadonlyArray<EgressHost>): Array<TagSelectOption> {
  const allowed = (installationHosts ?? []).map((host) => ({ key: webAccessHostKey(host), kind: "read" as const, label: webAccessHostKey(host) }));
  const dropped = unavailableWebAccessHosts(selectedHosts, installationHosts).map((host) => ({
    disabled: true,
    key: webAccessHostKey(host),
    kind: "read" as const,
    label: webAccessHostKey(host),
    note: "Not allowed in this installation",
  }));
  return [...allowed, ...dropped];
}

/** The selected hosts this installation doesn't allow. */
export function unavailableWebAccessHosts(selectedHosts: ReadonlyArray<EgressHost>, installationHosts: ReadonlyArray<EgressHost> | null | undefined): Array<EgressHost> {
  const allowed = hostKeys(installationHosts);
  return selectedHosts.filter((host) => !allowed.has(webAccessHostKey(host)));
}
