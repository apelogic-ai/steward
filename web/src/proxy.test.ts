import { afterEach, describe, expect, test } from "bun:test";
import { NextRequest } from "next/server";

import { proxy } from "./proxy";

const originalNodeEnv = process.env.NODE_ENV;
const mutableEnvironment = process.env as Record<string, string | undefined>;

afterEach(() => {
  mutableEnvironment.NODE_ENV = originalNodeEnv;
});

function policyFor(environment: "development" | "production"): string {
  mutableEnvironment.NODE_ENV = environment;
  const response = proxy(new NextRequest("https://steward.test/envelopes"));
  return response.headers.get("content-security-policy") ?? "";
}

function directive(policy: string, name: string): string {
  return policy.split("; ").find((value) => value.startsWith(`${name} `)) ?? "";
}

describe("Next content security policy", () => {
  test("development permits the eval and inline-style capabilities required by React and Turbopack", () => {
    const policy = policyFor("development");
    const scripts = directive(policy, "script-src");
    const styles = directive(policy, "style-src");
    expect(scripts).toContain("script-src 'self' 'nonce-");
    expect(scripts).toContain("'strict-dynamic' 'unsafe-eval'");
    expect(scripts).not.toContain("'unsafe-inline'");
    expect(styles).toContain("'unsafe-inline'");
    expect(styles).not.toContain("'nonce-");
  });

  test("production keeps eval and inline script execution forbidden", () => {
    const policy = policyFor("production");
    const scripts = directive(policy, "script-src");
    const styles = directive(policy, "style-src");
    expect(scripts).toContain("script-src 'self' 'nonce-");
    expect(scripts).toContain("'strict-dynamic'");
    expect(scripts).not.toContain("'unsafe-eval'");
    expect(scripts).not.toContain("'unsafe-inline'");
    expect(styles).toContain("'nonce-");
    expect(styles).not.toContain("'unsafe-inline'");
  });
});
