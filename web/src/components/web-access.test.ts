import { describe, expect, test } from "bun:test";

import type { BrowserEnvelope } from "@/api-client";
import { allowedWebAccessHosts, provisionedEnvelope, templateWebAccessOptions, unavailableWebAccessHosts, webAccessEmptyMessage, webAccessGrant, webAccessHostKey } from "./web-access";

const advisories = { host: "advisories.example.com", port: 443 };
const feeds = { host: "feeds.example.com", port: 8443 };
const retired = { host: "retired.example.com", port: 443 };

function templateEnvelope(): BrowserEnvelope {
  return {
    revision: 4,
    spec: {
      budget: { currency: "USD", monthlyLimit: "100.00", singleRunLimit: "5.00" },
      llms: [{ provider: "provider-a", model: "model-a" }],
      tools: [],
      ttl: "24h",
      runner: { platforms: ["linux"] },
      egress: { hosts: [advisories, feeds] },
    },
  };
}

describe("web access hosts", () => {
  test("keys a host by name and port, with HTTPS as the default port", () => {
    expect(webAccessHostKey(feeds)).toBe("feeds.example.com:8443");
    expect(webAccessHostKey({ host: "advisories.example.com" })).toBe("advisories.example.com:443");
  });

  test("offers only template hosts the installation still allows", () => {
    expect(allowedWebAccessHosts([advisories, retired, feeds], [feeds, advisories])).toEqual([advisories, feeds]);
    expect(allowedWebAccessHosts([advisories], undefined)).toEqual([]);
    expect(allowedWebAccessHosts(undefined, [advisories])).toEqual([]);
  });

  test("grants nothing until a host is chosen, then only the chosen hosts", () => {
    expect(webAccessGrant([advisories, feeds], new Set())).toBeUndefined();
    expect(webAccessGrant([advisories, feeds], new Set(["feeds.example.com:8443"]))).toEqual({ hosts: [feeds] });
  });

  test("provisions a template without web access unless the administrator picks hosts", () => {
    const template = templateEnvelope();
    const withoutHosts = provisionedEnvelope(template, new Set());
    expect(withoutHosts.spec.egress).toBeUndefined();
    const templateWithoutWebAccess = { ...template.spec };
    delete templateWithoutWebAccess.egress;
    expect(withoutHosts.spec).toEqual(templateWithoutWebAccess);
    expect(withoutHosts.revision).toBe(template.revision);
    expect(provisionedEnvelope(template, new Set(["advisories.example.com:443"])).spec.egress).toEqual({ hosts: [advisories] });
    expect(provisionedEnvelope(template, new Set(["unknown.example.com:443"])).spec.egress).toBeUndefined();
    expect(template.spec.egress).toEqual({ hosts: [advisories, feeds] });
  });

  test("lets a template editor add any allowed host and remove a host the installation dropped", () => {
    // A grant allows every HTTP method, so web access hosts carry no access-class badge.
    expect(templateWebAccessOptions([advisories, feeds], [retired])).toEqual([
      { key: "advisories.example.com:443", kind: "neutral", label: "advisories.example.com:443" },
      { key: "feeds.example.com:8443", kind: "neutral", label: "feeds.example.com:8443" },
      { disabled: true, key: "retired.example.com:443", kind: "neutral", label: "retired.example.com:443", note: "Not allowed in this installation" },
    ]);
    expect(unavailableWebAccessHosts([advisories, retired], [advisories, feeds])).toEqual([retired]);
    expect(unavailableWebAccessHosts([advisories], undefined)).toEqual([advisories]);
  });

  test("explains why no web access host is offered", () => {
    expect(webAccessEmptyMessage([advisories], [advisories], [advisories], false)).toBeNull();
    expect(webAccessEmptyMessage([], [], [advisories], false)).toBe("This template allows no web access.");
    expect(webAccessEmptyMessage(undefined, [], undefined, true)).toBe("This template allows no web access.");
    expect(webAccessEmptyMessage([retired], [], [advisories], false)).toBe("This installation no longer allows this template's web access hosts.");
    expect(webAccessEmptyMessage([advisories], [], undefined, true)).toBe("Loading web access hosts…");
    expect(webAccessEmptyMessage([advisories], [], undefined, false)).toBe("Web access hosts couldn't be loaded. Reload the page to try again.");
  });
});
