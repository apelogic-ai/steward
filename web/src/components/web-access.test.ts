import { describe, expect, test } from "bun:test";

import type { BrowserEnvelope } from "@/api-client";
import { parseWebAccessHost, provisionedEnvelope, webAccessGrant, webAccessHostKey, webAccessHostOptions } from "./web-access";

const advisories = { host: "advisories.example.com", port: 443 };
const feeds = { host: "feeds.example.com", port: 8443 };

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

  test("reads a typed host name, with HTTPS as the default port", () => {
    expect(parseWebAccessHost("advisories.example.com")).toEqual(advisories);
    expect(parseWebAccessHost("  Feeds.Example.com:8443 ")).toEqual(feeds);
    expect(parseWebAccessHost("never-listed.example.com:443")).toEqual({ host: "never-listed.example.com", port: 443 });
  });

  test("refuses anything that isn't an exact public host name and port", () => {
    for (const input of ["", "localhost", "example", "*.example.com", "https://api.example.com", "api.example.com/path", "192.0.2.10", "[2001:db8::1]:443", "api.example.com:0", "api.example.com:65536", "api.example.com:http", "-api.example.com", "api_example.com", "api.example.123", "api..example.com"]) {
      expect(parseWebAccessHost(input)).toBeNull();
    }
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

  test("shows every host as a neutral option", () => {
    // A grant allows every HTTP method, so web access hosts carry no access-class badge.
    expect(webAccessHostOptions([advisories, feeds])).toEqual([
      { key: "advisories.example.com:443", kind: "neutral", label: "advisories.example.com:443" },
      { key: "feeds.example.com:8443", kind: "neutral", label: "feeds.example.com:8443" },
    ]);
  });
});
