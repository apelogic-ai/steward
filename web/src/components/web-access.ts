import type { BrowserEnvelope, EgressAuthority, EgressHost } from "@/api-client";
import type { TagSelectOption } from "@/components/hs";

/** The `host:port` key the browser uses for one web access host. */
export function webAccessHostKey(host: EgressHost): string {
  return `${host.host}:${host.port ?? 443}`;
}

const DNS_LABEL = /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/;

/**
 * Reads a typed web access host: an exact public DNS name, with `:port` when it isn't 443. Returns
 * null for anything else (wildcards, IP addresses, URLs, `localhost`, port 0). The server checks
 * the same rules.
 */
export function parseWebAccessHost(input: string): EgressHost | null {
  const match = /^([^:]+)(?::(\d{1,5}))?$/.exec(input.trim().toLowerCase());
  if (!match) return null;
  const [, host, portText] = match;
  const port = portText === undefined ? 443 : Number(portText);
  if (port < 1 || port > 65535 || host.length > 253) return null;
  const labels = host.split(".");
  if (labels.length < 2 || !labels.every((label) => DNS_LABEL.test(label))) return null;
  const topLevel = labels[labels.length - 1];
  if (!/^[a-z]/.test(topLevel) || topLevel === "localhost") return null;
  return { host, port };
}

/** Every host as a neutral option: a grant allows every HTTP method, so hosts carry no access-class badge. */
export function webAccessHostOptions(hosts: ReadonlyArray<EgressHost>): Array<TagSelectOption> {
  return hosts.map((host) => ({ key: webAccessHostKey(host), kind: "neutral" as const, label: webAccessHostKey(host) }));
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
