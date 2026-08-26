import { spawn } from "node:child_process";
import { createServer } from "node:http";
import net from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { expect, test } from "@playwright/test";

const repository = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const webDirectory = path.join(repository, "web");
const nextBinary = path.join(repository, "node_modules", "next", "dist", "bin", "next");
const STARTUP_TIMEOUT_MS = 30_000;
const envelopeId = "00000000-0000-0000-0000-000000000001";
const taskUid = "00000000-0000-0000-0000-000000000002";
const approvalId = "00000000-0000-0000-0000-000000000003";
let web;
let origin;

const developerSession = {
  apiVersion: "steward.browser-session/v1",
  principal: {
    userId: "usr_abcdef0123456789abcdef0123456789",
    displayName: "Alice Example",
    displayEmail: "alice@example.com",
  },
  role: "user",
  memberRoles: ["developer"],
  surfaces: ["connections", "envelopeRequests", "agentRuns"],
  csrf: "test-csrf",
};

const administratorSession = {
  ...developerSession,
  role: "admin",
  memberRoles: ["analyst"],
};

const previousSessionContract = {
  ...developerSession,
  principal: {
    userId: developerSession.principal.userId,
    displayEmail: developerSession.principal.displayEmail,
  },
};

const envelope = {
  revision: 4,
  spec: {
    budget: { currency: "USD", monthlyLimit: "25.00", singleRunLimit: "5.00" },
    llms: [{ provider: "provider-a", model: "model-a" }],
    tools: [{ provider: "github", resource: "repository", action: "get_file_contents" }],
    ttl: "4h",
    runner: { platforms: [] },
  },
};

const analystEnvelope = {
  revision: 2,
  spec: {
    budget: { currency: "USD", monthlyLimit: "10.00", singleRunLimit: "1.00" },
    llms: [{ provider: "provider-b", model: "model-b" }],
    tools: [{ provider: "github", resource: "repository", action: "list_issues" }],
    ttl: "2h",
    runner: { platforms: ["linux"] },
  },
};

const adminEnvelope = {
  ...envelope,
  spec: {
    ...envelope.spec,
    budget: { ...envelope.spec.budget, singleRunLimit: "2.50" },
    llms: [{ provider: "openai", model: "gpt-5.4" }],
  },
};

const envelopeRequest = {
  id: envelopeId,
  templateId: "developer",
  templateRevision: 4,
  requestedEnvelope: envelope,
  approvedEnvelope: envelope,
  status: "provisioned",
  approvalId: null,
  envelopeInstanceId: "runtime-example-1",
  envelopeDigest: "sha256:example",
  reason: null,
  createdAt: "2026-08-24T17:00:00Z",
  statusAt: "2026-08-24T17:01:00Z",
};

const pendingEnvelopeRequest = {
  id: "00000000-0000-0000-0000-000000000004",
  ownerUserId: developerSession.principal.userId,
  ownerDisplayEmail: developerSession.principal.displayEmail,
  templateId: "developer",
  templateRevision: 4,
  requestedEnvelope: envelope,
  createdAt: "2026-08-24T17:05:00Z",
};

const run = {
  taskUid,
  workflow: "repository-review@1",
  workflowName: "repository-review",
  workflowVersion: 1,
  workflowDigest: `sha256:${"a".repeat(64)}`,
  userEnvelopeInstanceId: "runtime-example-1",
  userEnvelopeRevision: 4,
  userEnvelopeDigest: `sha256:${"b".repeat(64)}`,
  codingAgentRuntime: "codex@0.117.0",
  runtimeUid: "runtime-example-1",
  runtimeOwnership: "provisioned",
  phase: "succeeded",
  envelopeRevision: 4,
  finalizationRequested: true,
  finalized: true,
  createdAt: "2026-08-24T17:02:00Z",
  updatedAt: "2026-08-24T17:03:00Z",
  observedSpend: { observedAmount: "1.25", currency: "USD", exhausted: false },
  errorCategory: null,
};

const workflowRevision = {
  name: "repository-review",
  version: 1,
  displayName: "Repository review",
  agent: "codex@0.117.0",
  prompt: "Review the repository state that triggered this GitHub Actions run.",
  contentDigest: `sha256:${"c".repeat(64)}`,
  publishedBy: developerSession.principal.userId,
  publishedAt: "2026-08-24T16:55:00Z",
};

const approval = {
  approvalId,
  runtimeUid: "runtime-example-2",
  memberRole: "analyst",
  actor: "alice@example.com",
  envelopeRevision: 4,
  counterexample: "budget.monthly_limit: requested 40.00 exceeds 25.00",
  proposedSpec: {
    agentType: { name: "coding-agent" },
    owner: "alice@example.com",
    principal: { kind: "user", acting_user: "alice@example.com" },
    budget: { currency: "USD", monthlyLimit: "40.00" },
    llms: [{ provider: "provider-a", model: "model-a" }],
    tools: [{ provider: "github", resource: "repository", action: "get_file_contents" }],
    ttl: "4h",
    runner: { platforms: [] },
  },
  decisionKey: null,
  evidenceUrl: null,
};

const presentationRoutes = [
  { path: "/envelopes", heading: "Envelopes", activeNavigation: "Envelopes" },
  { path: "/envelopes/new", heading: "New envelope", activeNavigation: "Envelopes" },
  { path: `/envelopes/${envelopeId}`, heading: "Envelope", activeNavigation: "Envelopes" },
  { path: `/envelopes/${envelopeId}/runs`, heading: "Recent runs", activeNavigation: "Envelopes" },
  { path: "/runs", heading: "Runs", activeNavigation: "Runs" },
  { path: `/runs/${taskUid}`, heading: "Run detail", activeNavigation: "Runs" },
  { path: "/connections", heading: "Connections", activeNavigation: "Connections" },
  { path: "/settings", heading: "Settings", activeNavigation: "Settings" },
  { path: "/admin/envelopes/templates", heading: "Envelope templates", activeNavigation: "Templates" },
  { path: "/admin/envelopes/templates/analyst", heading: "Envelope template", activeNavigation: "Templates" },
  { path: "/admin/workflows", heading: "Workflows", activeNavigation: "Workflows" },
  { path: "/admin/workflows/new", heading: "New workflow", activeNavigation: "Workflows" },
  { path: "/admin/workflows/repository-review/versions/1", heading: "Workflow", activeNavigation: "Workflows" },
  { path: "/admin/workflows/repository-review/new-version", heading: "New repository-review version", activeNavigation: "Workflows" },
  { path: "/admin/runs", heading: "All runs", activeNavigation: "Runs" },
  { path: `/admin/runs/${taskUid}`, heading: "Run detail", activeNavigation: "Runs" },
  { path: "/admin/approvals", heading: "Pending approvals", activeNavigation: "Approvals" },
  { path: "/admin/settings", heading: "Settings", activeNavigation: "Settings" },
];

function reservePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.unref();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      if (!address || typeof address === "string") {
        server.close();
        reject(new Error("could not reserve a loopback port for steward-web"));
        return;
      }
      server.close((error) => error ? reject(error) : resolve(address.port));
    });
  });
}

async function startWeb() {
  const nextPort = await reservePort();
  const child = spawn(process.execPath, [nextBinary, "start", "--port", String(nextPort)], {
    cwd: webDirectory,
    stdio: ["ignore", "pipe", "pipe"],
  });
  let output = "";
  child.stdout.on("data", (chunk) => { output = `${output}${chunk}`.slice(-16_384); });
  child.stderr.on("data", (chunk) => { output = `${output}${chunk}`.slice(-16_384); });
  const nextOrigin = `http://127.0.0.1:${nextPort}`;
  const deadline = Date.now() + STARTUP_TIMEOUT_MS;
  let ready = false;
  while (Date.now() < deadline) {
    if (child.exitCode !== null || child.signalCode !== null) {
      throw new Error(`steward-web exited before readiness:\n${output}`);
    }
    try {
      const response = await fetch(`${nextOrigin}/health/ready`, { cache: "no-store" });
      if (response.status === 204) {
        ready = true;
        break;
      }
    } catch {
      // The loopback listener is still starting.
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  if (!ready) {
    child.kill("SIGTERM");
    throw new Error(`steward-web did not become ready:\n${output}`);
  }

  let mutationFailures = {};
  let mutationSink;
  const proxy = createServer(async (request, response) => {
    try {
      const requestUrl = new URL(request.url ?? "/", nextOrigin);
      const browserMutation = request.method === "POST" && (
        requestUrl.pathname === "/app/api/v1/envelope-requests"
        || requestUrl.pathname.endsWith("/github-actions-workflow")
        || requestUrl.pathname.startsWith("/admin/api/v1/envelope-templates/")
        || requestUrl.pathname === "/admin/api/v1/workflows"
        || requestUrl.pathname.endsWith("/versions")
        || requestUrl.pathname === `/admin/api/v1/approvals/${approvalId}/approve`
        || requestUrl.pathname === `/admin/api/v1/approvals/${approvalId}/file`
        || requestUrl.pathname === "/admin/api/v1/connections/github/start"
        || requestUrl.pathname === "/admin/api/v1/connections/github/disconnect"
        || requestUrl.pathname === "/admin/auth/logout"
      );
      if (browserMutation) {
        const chunks = [];
        for await (const chunk of request) chunks.push(chunk);
        const rawBody = Buffer.concat(chunks).toString("utf8");
        mutationSink?.push({
          path: requestUrl.pathname,
          headers: request.headers,
          body: rawBody ? JSON.parse(rawBody) : null,
        });
        const failureStatus = mutationFailures[requestUrl.pathname];
        if (failureStatus) {
          response.writeHead(failureStatus, { "content-type": "application/json", "cache-control": "no-store" });
          response.end("{}");
          return;
        }
        if (requestUrl.pathname.startsWith("/admin/api/v1/envelope-templates/")) {
          const memberRole = decodeURIComponent(requestUrl.pathname.split("/").at(-1) ?? "");
          response.writeHead(201, { "content-type": "application/json", "cache-control": "no-store" });
          response.end(JSON.stringify({ apiVersion: "steward.browser-admin/v1", memberRole, envelope: JSON.parse(rawBody) }));
          return;
        }
        if (requestUrl.pathname === "/admin/api/v1/workflows" || requestUrl.pathname.endsWith("/versions")) {
          const submitted = JSON.parse(rawBody);
          response.writeHead(201, { "content-type": "application/json", "cache-control": "no-store" });
          response.end(JSON.stringify({
            apiVersion: "steward.workflows/v1",
            workflow: {
              ...workflowRevision,
              ...submitted,
              name: submitted.name ?? workflowRevision.name,
              version: requestUrl.pathname.endsWith("/versions") ? 2 : 1,
            },
          }));
          return;
        }
        if (requestUrl.pathname.endsWith("/github-actions-workflow")) {
          response.writeHead(200, { "content-type": "application/json", "cache-control": "no-store" });
          response.end(JSON.stringify({ apiVersion: "steward.envelope-requests/v1", workflow: { schemaVersion: "v2", contentType: "application/yaml", sha256: "abc123", yaml: ["name: Steward governed run", "on:", "  workflow_dispatch:", "jobs:", "  governed:", "    with:", "      workflow: repository-review@1", ""].join("\n") } }));
          return;
        }
        if (requestUrl.pathname.endsWith("/approve")) {
          response.writeHead(204, { "cache-control": "no-store" });
          response.end();
          return;
        }
        if (requestUrl.pathname.endsWith("/file")) {
          response.writeHead(200, { "content-type": "application/json", "cache-control": "no-store" });
          response.end(JSON.stringify({ apiVersion: "steward.browser-admin/v1", approvalId, decisionKey: "PROJ-123", evidenceUrl: "https://example.com/decisions/PROJ-123" }));
          return;
        }
        if (requestUrl.pathname.endsWith("/connections/github/start")) {
          response.writeHead(200, { "content-type": "application/json", "cache-control": "no-store" });
          response.end(JSON.stringify({ apiVersion: "steward.connections/v1", provider: "github", authorizationUrl: `${web.origin}/connections?oauth=started` }));
          return;
        }
        if (requestUrl.pathname.endsWith("/connections/github/disconnect")) {
          response.writeHead(204, { "cache-control": "no-store" });
          response.end();
          return;
        }
        if (requestUrl.pathname === "/admin/auth/logout") {
          response.writeHead(204, { "cache-control": "no-store" });
          response.end();
          return;
        }
        response.writeHead(201, { "content-type": "application/json", "cache-control": "no-store" });
        response.end(JSON.stringify({ apiVersion: "steward.envelope-requests/v1", request: envelopeRequest }));
        return;
      }

      const headers = new Headers();
      for (const [name, value] of Object.entries(request.headers)) {
        if (!value || ["accept-encoding", "connection", "content-length", "host"].includes(name)) continue;
        for (const item of Array.isArray(value) ? value : [value]) headers.append(name, item);
      }
      const upstream = await fetch(requestUrl, { method: request.method, headers });
      response.statusCode = upstream.status;
      for (const [name, value] of upstream.headers) {
        if (!["connection", "content-encoding", "content-length", "transfer-encoding"].includes(name)) {
          response.setHeader(name, value);
        }
      }
      response.end(Buffer.from(await upstream.arrayBuffer()));
    } catch (error) {
      response.writeHead(502, { "content-type": "text/plain" });
      response.end(error instanceof Error ? error.message : "test proxy failed");
    }
  });
  const proxyPort = await reservePort();
  await new Promise((resolve, reject) => {
    proxy.once("error", reject);
    proxy.listen(proxyPort, "127.0.0.1", resolve);
  });
  return {
    child,
    proxy,
    origin: `http://127.0.0.1:${proxyPort}`,
    output: () => output,
    useMutationFailures: (failures) => { mutationFailures = failures; },
    useMutationSink: (sink) => { mutationSink = sink; },
  };
}

function hasExited(child) {
  return child.exitCode !== null || child.signalCode !== null;
}

function waitForExit(child, timeoutMs) {
  if (hasExited(child)) return Promise.resolve(true);
  return new Promise((resolve) => {
    const timeout = setTimeout(() => {
      child.removeListener("exit", onExit);
      resolve(false);
    }, timeoutMs);
    const onExit = () => {
      clearTimeout(timeout);
      resolve(true);
    };
    child.once("exit", onExit);
  });
}

async function stopWeb(instance) {
  if (!instance) return;
  await new Promise((resolve) => instance.proxy.close(resolve));
  const { child } = instance;
  if (hasExited(child)) return;
  child.kill("SIGTERM");
  if (await waitForExit(child, 5_000)) return;
  child.kill("SIGKILL");
  if (!(await waitForExit(child, 5_000))) {
    throw new Error(`steward-web did not exit after SIGKILL (pid ${child.pid})`);
  }
}

async function guardedPage(browser, {
  legacyAdminTemplate = false,
  malformedAdminTemplate = false,
  mockSignIn = true,
  colorScheme = "dark",
  connectionPhase = "connected",
  emptyCollections = false,
  expectedHttpStatuses = [],
  mutationFailures = {},
  session = developerSession,
  viewport = { width: 1280, height: 800 },
} = {}) {
  const context = await browser.newContext({ colorScheme, viewport });
  const mutations = [];
  web.useMutationFailures(mutationFailures);
  web.useMutationSink(mutations);
  await context.addInitScript(() => {
    const allowedPreference = (key) => typeof key === "string" && key.startsWith("steward.ui.envelope-accordion.");
    for (const method of ["getItem", "removeItem", "setItem"]) {
      const original = Storage.prototype[method];
      Object.defineProperty(Storage.prototype, method, {
        configurable: true,
        value(...args) {
          if (!allowedPreference(args[0])) throw new Error("Steward may persist only envelope accordion preferences");
          return original.apply(this, args);
        },
      });
    }
    for (const method of ["clear", "key"]) {
      Object.defineProperty(Storage.prototype, method, {
        configurable: true,
        value() { throw new Error("Steward must not enumerate or clear browser storage"); },
      });
    }
  });
  await context.route(`${origin}/admin/api/v1/session`, async (route) => {
    if (session === null) {
      await route.fulfill({ status: 401, body: "" });
      return;
    }
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify(session),
    });
  });
  await context.route(`${origin}/admin/auth/login*`, (route) => route.fulfill({
    status: 200,
    contentType: "text/html",
    body: "<!doctype html><title>Rust authentication start</title><h1>Rust authentication start</h1>",
  }));
  if (mockSignIn) {
    await context.route(`${origin}/admin/sign-in`, (route) => route.fulfill({
      status: 200,
      contentType: "text/html",
      body: "<!doctype html><title>Signed out</title><h1>Signed out</h1>",
    }));
  }
  const json = (route, body, status = 200) => route.fulfill({ status, contentType: "application/json", body: JSON.stringify(body) });
  await context.route(`${origin}/app/api/v1/envelope-templates`, (route) => json(route, {
    apiVersion: "steward.envelope-requests/v1",
    templates: emptyCollections ? [] : [
      { id: "analyst", displayName: "Analyst", revision: 2, ceiling: analystEnvelope, autoProvisionThreshold: null, githubConnection: "connected" },
      { id: "developer", displayName: "Developer", revision: 4, ceiling: envelope, autoProvisionThreshold: null, githubConnection: "connected" },
    ],
  }));
  await context.route(`${origin}/app/api/v1/workflows`, (route) => json(route, {
    apiVersion: "steward.workflows/v1",
    workflows: emptyCollections ? [] : [{
      agent: workflowRevision.agent,
      displayName: workflowRevision.displayName,
      name: workflowRevision.name,
      version: workflowRevision.version,
    }],
  }));
  await context.route(`${origin}/app/api/v1/envelope-requests`, async (route) => {
    if (route.request().method() === "POST") {
      await route.continue();
      return;
    }
    await json(route, { apiVersion: "steward.envelope-requests/v1", requests: emptyCollections ? [] : [envelopeRequest] });
  });
  await context.route(`${origin}/app/api/v1/envelope-requests/**`, async (route) => {
    if (route.request().url().endsWith("/github-actions-workflow")) {
      await route.continue();
      return;
    }
    await json(route, { apiVersion: "steward.envelope-requests/v1", request: envelopeRequest });
  });
  await context.route(`${origin}/app/api/v1/runs*`, (route) => json(route, { apiVersion: "steward.browser-runs/v1", runs: emptyCollections ? [] : [run], nextCursor: null }));
  await context.route(`${origin}/app/api/v1/runs/**`, (route) => route.request().url().endsWith("/timeline")
    ? json(route, { apiVersion: "steward.browser-runs/v1", taskUid, events: emptyCollections ? [] : [{ kind: "phase", phase: "succeeded", at: "2026-08-24T17:03:00Z" }] })
    : json(route, { apiVersion: "steward.browser-runs/v1", run }));
  await context.route(`${origin}/admin/api/v1/all-runs*`, (route) => json(route, { apiVersion: "steward.browser-runs/v1", runs: emptyCollections ? [] : [{ ...run, ownerUserId: developerSession.principal.userId }], nextCursor: null }));
  await context.route(`${origin}/admin/api/v1/all-runs/**`, (route) => route.request().url().endsWith("/timeline")
    ? json(route, { apiVersion: "steward.browser-runs/v1", taskUid, events: emptyCollections ? [] : [{ kind: "phase", phase: "succeeded", at: "2026-08-24T17:03:00Z" }] })
    : json(route, { apiVersion: "steward.browser-runs/v1", run }));
  await context.route(`${origin}/admin/api/v1/envelope-templates/**`, async (route) => {
    if (route.request().method() === "POST") {
      await route.continue();
      return;
    }
    const memberRole = decodeURIComponent(new URL(route.request().url()).pathname.split("/").at(-1) ?? "");
    await json(route, malformedAdminTemplate
      ? { apiVersion: "steward.browser-admin/v1", memberRole }
      : legacyAdminTemplate
        ? { apiVersion: "steward.admin/v1", template: { id: memberRole, revision: 4, envelope } }
        : { apiVersion: "steward.browser-admin/v1", memberRole, envelope: adminEnvelope });
  });
  await context.route(`${origin}/admin/api/v1/envelope-templates`, (route) => json(route, {
    apiVersion: "steward.browser-admin/v1",
    templates: emptyCollections ? [] : [
      { memberRole: "analyst", envelope: adminEnvelope },
      { memberRole: "developer", envelope },
    ],
  }));
  await context.route(`${origin}/admin/api/v1/workflows`, async (route) => {
    if (route.request().method() === "POST") {
      await route.continue();
      return;
    }
    await json(route, {
      apiVersion: "steward.workflows/v1",
      workflows: emptyCollections ? [] : [workflowRevision],
    });
  });
  await context.route(`${origin}/admin/api/v1/workflows/**`, async (route) => {
    if (route.request().method() === "POST") {
      await route.continue();
      return;
    }
    await json(route, { apiVersion: "steward.workflows/v1", workflow: workflowRevision });
  });
  await context.route(`${origin}/admin/api/v1/approvals`, (route) => json(route, {
    apiVersion: "steward.browser-admin/v1",
    approvals: emptyCollections ? [] : [approval],
    envelopeRequests: emptyCollections ? [] : [pendingEnvelopeRequest],
  }));
  await context.route(`${origin}/admin/api/v1/approvals/**`, (route) => route.continue());
  await context.route(`${origin}/admin/api/v1/connections/github`, (route) => json(route, {
    apiVersion: "steward.connections/v1",
    provider: "github",
    status: connectionPhase === "connected"
      ? { phase: "connected", accountEmail: "alice@example.com", scopesRequired: ["repo"], scopesGranted: ["repo"], scopesMissing: [], expiresAt: null }
      : { phase: connectionPhase, accountEmail: null, scopesRequired: ["repo"], scopesGranted: [], scopesMissing: ["repo"], expiresAt: null },
  }));
  await context.route(`${origin}/admin/api/v1/connections/github/*`, (route) => route.continue());
  const page = await context.newPage();
  const consoleErrors = [];
  const crossOriginRequests = [];
  const httpErrors = [];
  page.on("console", (message) => {
    const expectedUnauthorizedProbe = session === null
      && message.type() === "error"
      && message.text() === "Failed to load resource: the server responded with a status of 401 (Unauthorized)";
    const expectedMutationFailure = expectedHttpStatuses.some((status) => message.text().includes(`status of ${status}`));
    if (message.type() === "error" && !expectedUnauthorizedProbe && !expectedMutationFailure) consoleErrors.push(message.text());
  });
  page.on("pageerror", (error) => consoleErrors.push(error.message));
  page.on("response", (response) => {
    const expectedUnauthorizedProbe = session === null && response.url().endsWith("/admin/api/v1/session") && response.status() === 401;
    const expectedMutationFailure = expectedHttpStatuses.includes(response.status());
    if (response.status() >= 400 && !expectedUnauthorizedProbe && !expectedMutationFailure) httpErrors.push(`${response.status()} ${response.url()}`);
  });
  page.on("request", (request) => {
    if (new URL(request.url()).origin !== origin) crossOriginRequests.push(request.url());
  });
  return { context, page, consoleErrors, crossOriginRequests, httpErrors, mutations };
}

async function closeGuardedPage(session) {
  try {
    expect(session.httpErrors, "unexpected HTTP failures must fail the Next gate").toEqual([]);
    expect(session.consoleErrors, "browser console errors must fail the Next gate").toEqual([]);
    expect(session.crossOriginRequests, "the initial migration must make same-origin requests only").toEqual([]);
  } finally {
    await session.context.close();
  }
}

function expectMutationProof(mutation) {
  expect(mutation, "expected browser mutation was not observed at the same-origin boundary").toBeTruthy();
  expect(mutation.headers["x-steward-csrf"]).toBe("test-csrf");
  expect(mutation.headers["content-type"]).toContain("application/json");
  expect(mutation.headers.origin).toBe(origin);
  expect(mutation.headers["sec-fetch-site"]).toBe("same-origin");
}

test.beforeAll(async () => {
  web = await startWeb();
  origin = web.origin;
});

test.afterAll(async () => {
  await stopWeb(web);
});

test("Next pages carry one strict nonce and nested developer navigation", async ({ browser }) => {
  const session = await guardedPage(browser);
  try {
    const response = await session.page.goto(`${origin}/envelopes/new`);
    expect(response?.status()).toBe(200);
    const policy = response?.headers()["content-security-policy"] ?? "";
    expect(policy).toContain("script-src 'self' 'nonce-");
    expect(policy).toContain("'strict-dynamic'");
    expect(policy).toContain("style-src 'self' 'nonce-");
    expect(policy).toContain("connect-src 'self'");
    expect(policy).not.toContain("'unsafe-inline'");
    expect(policy).not.toContain("'unsafe-eval'");

    const nonce = policy.match(/script-src 'self' 'nonce-([^']+)'/)?.[1];
    expect(nonce).toBeTruthy();
    const scriptNonces = await session.page.locator("script").evaluateAll((scripts) => scripts.map((script) => script.nonce));
    expect(scriptNonces.length).toBeGreaterThan(0);
    expect(scriptNonces.every((value) => value === nonce)).toBe(true);

    await session.page.getByRole("button", { name: "Account menu" }).click();
    await expect(session.page.getByRole("menu", { name: "Account" }).getByText("Alice Example", { exact: true })).toBeVisible();
    await session.page.getByRole("button", { name: "Account menu" }).click();
    await expect(session.page.getByRole("link", { name: "Envelopes", exact: true })).toHaveAttribute("aria-current", "page");
    await session.page.getByRole("link", { name: "Runs", exact: true }).click();
    await expect(session.page).toHaveURL(`${origin}/runs`);
    await expect(session.page.getByRole("link", { name: "Runs", exact: true })).toHaveAttribute("aria-current", "page");
  } finally {
    await closeGuardedPage(session);
  }
});

test("the shell carries the ApeLogic visual system from the db-mcp web app", async ({ browser }) => {
  const session = await guardedPage(browser);
  try {
    await session.page.goto(`${origin}/envelopes`);
    await expect(session.page.getByRole("img", { name: "ApeLogic" })).toHaveAttribute("src", "/icon.svg");
    await expect(session.page.getByRole("link", { name: /ApeLogic Steward/ })).toHaveAttribute("href", "/envelopes");
    await expect(session.page.locator("link[rel='icon'][href*='favicon.ico']")).toHaveCount(1);
    const favicon = await session.page.request.get(`${origin}/favicon.ico`);
    expect(favicon.status()).toBe(200);
    expect(favicon.headers()["content-type"]).toContain("image/x-icon");

    const brand = await session.page.evaluate(() => {
      const body = getComputedStyle(document.body);
      const header = getComputedStyle(document.querySelector("body > div > header"));
      const primary = getComputedStyle(document.querySelector("a[href='/envelopes/new']"));
      return {
        background: body.backgroundColor,
        foreground: body.color,
        font: body.fontFamily,
        headerBorder: header.borderBottomColor,
        primary: primary.backgroundColor,
      };
    });
    expect(brand).toEqual({
      background: "rgb(18, 18, 18)",
      foreground: "rgb(250, 250, 250)",
      font: expect.stringContaining("Space Grotesk"),
      headerBorder: "rgb(46, 46, 46)",
      primary: "rgb(239, 134, 38)",
    });
  } finally {
    await closeGuardedPage(session);
  }
});

test("the administrator root preserves old bookmarks by entering the authorized workspace", async ({ browser }) => {
  const administrator = await guardedPage(browser, { session: administratorSession });
  try {
    await administrator.page.goto(`${origin}/admin`);
    await expect(administrator.page).toHaveURL(`${origin}/admin/runs`);
    await expect(administrator.page.getByRole("heading", { name: "All runs", exact: true })).toBeVisible();
  } finally {
    await closeGuardedPage(administrator);
  }
});

test("a user session cannot enter the administrator workspace", async ({ browser }) => {
  const developer = await guardedPage(browser);
  try {
    await developer.page.goto(`${origin}/admin/runs`);
    await expect(developer.page.getByRole("heading", { name: "Forbidden", exact: true })).toBeVisible();
    await expect(developer.page.getByRole("navigation", { name: "Primary navigation" })).toHaveCount(0);
    await expect(developer.page.getByRole("link", { name: "Templates", exact: true })).toHaveCount(0);
    await expect(developer.page.getByRole("heading", { name: "All runs", exact: true })).toHaveCount(0);
  } finally {
    await closeGuardedPage(developer);
  }
});

test("every presentation route sends unauthenticated sessions to the Rust auth start", async ({ browser }) => {
  const unauthorized = await guardedPage(browser, { session: null });
  try {
    for (const route of presentationRoutes) {
      await test.step(route.path, async () => {
        await unauthorized.page.goto(`${origin}${route.path}`);
        const exactReturn = ["/connections", "/envelopes", "/envelopes/new", "/runs", "/settings"].includes(route.path)
          ? route.path
          : route.path.startsWith("/runs/") ? "/runs" : "/envelopes";
        await expect(unauthorized.page).toHaveURL(`${origin}/admin/auth/login?returnTo=${encodeURIComponent(exactReturn)}`);
        await expect(unauthorized.page.getByRole("heading", { name: "Rust authentication start" })).toBeVisible();
      });
    }
  } finally {
    await closeGuardedPage(unauthorized);
  }
});

test("an expired session response restarts the Rust auth flow", async ({ browser }) => {
  const expired = await guardedPage(browser, {
    expectedHttpStatuses: [401],
    mutationFailures: { "/app/api/v1/envelope-requests": 401 },
  });
  try {
    await expired.page.goto(`${origin}/envelopes/new`);
    await expired.page.getByRole("button", { name: "Submit request" }).click();
    await expect(expired.page).toHaveURL(`${origin}/admin/auth/login?returnTo=%2Fenvelopes%2Fnew`);
    await expect(expired.page.getByRole("heading", { name: "Rust authentication start" })).toBeVisible();
  } finally {
    await closeGuardedPage(expired);
  }
});

test("the account menu identifies the user and exposes only server-authorized workspaces", async ({ browser }) => {
  const developer = await guardedPage(browser, { colorScheme: "light" });
  try {
    await developer.page.goto(`${origin}/envelopes`);
    const accountButton = developer.page.getByRole("button", { name: "Account menu" });
    await expect(accountButton).toHaveText("A");
    const accountButtonBox = await accountButton.boundingBox();
    expect(accountButtonBox?.width).toBe(accountButtonBox?.height);
    expect(accountButtonBox?.width ?? 0).toBeGreaterThanOrEqual(40);
    await accountButton.click();
    const account = developer.page.getByRole("menu", { name: "Account" });
    await expect(account.getByText("Alice Example", { exact: true })).toBeVisible();
    await expect(account.getByText("alice@example.com", { exact: true })).toBeVisible();
    await expect(account.getByLabel("Workspace view")).toHaveCount(0);
    await expect(account.getByText("Mode", { exact: true })).toBeVisible();
    await expect(account.getByText("APPEARANCE", { exact: true })).toHaveCount(0);
    await account.getByRole("button", { name: "Switch to dark mode" }).click();
    await expect(developer.page.locator("html")).toHaveAttribute("data-theme", "dark");
    await expect(account.getByRole("button", { name: "Switch to light mode" })).toBeVisible();

    const logoutButton = account.getByRole("button", { name: "Log out" });
    await expect(logoutButton).toHaveCSS("background-color", "rgb(239, 134, 38)");
    await expect(logoutButton.locator("xpath=..")).toHaveCSS("border-top-style", "solid");
    await logoutButton.click();
    await expect(developer.page).toHaveURL(`${origin}/admin/sign-in`);
    await expect(developer.page.getByRole("heading", { name: "Signed out" })).toBeVisible();
    const logout = developer.mutations.find((mutation) => mutation.path === "/admin/auth/logout");
    expectMutationProof(logout);
    expect(logout.body).toEqual({});
  } finally {
    await closeGuardedPage(developer);
  }

  const dualRole = await guardedPage(browser, { session: administratorSession });
  try {
    await dualRole.page.goto(`${origin}/envelopes`);
    await dualRole.page.getByRole("button", { name: "Account menu" }).click();
    const account = dualRole.page.getByRole("menu", { name: "Account" });
    const workspace = account.getByLabel("Workspace view");
    await expect(workspace).toHaveValue("user");
    await expect(workspace.locator("option")).toHaveText(["User", "Admin"]);
    await expect(workspace.locator("xpath=..")).toHaveCSS("border-top-style", "solid");
    await workspace.selectOption("admin");
    await expect(dualRole.page).toHaveURL(`${origin}/admin/envelopes/templates`);
    await dualRole.page.getByRole("button", { name: "Account menu" }).click();
    await expect(dualRole.page.getByRole("menu", { name: "Account" }).getByLabel("Workspace view")).toHaveValue("admin");
    await expect(dualRole.page.getByRole("link", { name: "Templates", exact: true })).toHaveAttribute("aria-current", "page");
    await dualRole.page.getByRole("button", { name: "Account menu" }).click();
    await dualRole.page.goBack();
    await expect(dualRole.page).toHaveURL(`${origin}/envelopes`);
    await dualRole.page.getByRole("button", { name: "Account menu" }).click();
    await expect(dualRole.page.getByRole("menu", { name: "Account" }).getByLabel("Workspace view")).toHaveValue("user");
    await dualRole.page.getByRole("button", { name: "Account menu" }).click();
    await dualRole.page.goForward();
    await expect(dualRole.page).toHaveURL(`${origin}/admin/envelopes/templates`);
  } finally {
    await closeGuardedPage(dualRole);
  }
});

test("logout never restarts authentication when the server session has already expired", async ({ browser }) => {
  const expired = await guardedPage(browser, {
    expectedHttpStatuses: [401],
    mutationFailures: { "/admin/auth/logout": 401 },
  });
  try {
    await expired.page.goto(`${origin}/envelopes`);
    await expired.page.getByRole("button", { name: "Account menu" }).click();
    await expired.page.getByRole("button", { name: "Log out" }).click();

    await expect(expired.page).toHaveURL(`${origin}/admin/sign-in`);
    await expect(expired.page.getByRole("heading", { name: "Signed out" })).toBeVisible();
  } finally {
    await closeGuardedPage(expired);
  }
});

test("the signed-out page does not immediately start a new Google session", async ({ browser }) => {
  const signedOut = await guardedPage(browser, {
    expectedHttpStatuses: [401],
    mockSignIn: false,
    session: null,
  });
  try {
    await signedOut.page.goto(`${origin}/admin/sign-in`);

    await expect(signedOut.page).toHaveURL(`${origin}/admin/sign-in`);
    await expect(signedOut.page.getByRole("heading", { name: "Sign in required" })).toBeVisible();
    await expect(signedOut.page.getByRole("link", { name: "Continue with Google" })).toBeVisible();
  } finally {
    await closeGuardedPage(signedOut);
  }
});

test("a rolling local session contract falls back to the authenticated email without crashing", async ({ browser }) => {
  const session = await guardedPage(browser, { session: previousSessionContract });
  try {
    await session.page.goto(`${origin}/envelopes`);
    await session.page.getByRole("button", { name: "Account menu" }).click();
    const email = session.page.getByRole("menu", { name: "Account" }).getByText("alice@example.com", { exact: true });
    await expect(email).toBeVisible();
    await expect(email).toHaveCSS("font-weight", "400");
  } finally {
    await closeGuardedPage(session);
  }
});

test("every presentation route remains navigable at a narrow viewport", async ({ browser }) => {
  const session = await guardedPage(browser, { session: administratorSession, viewport: { width: 375, height: 812 } });
  try {
    for (const route of presentationRoutes) {
      await test.step(route.path, async () => {
        await session.page.goto(`${origin}${route.path}`);
        await expect(session.page.getByRole("heading", { name: route.heading, exact: true }).first()).toBeVisible();
        await expect(session.page.getByRole("link", { name: route.activeNavigation, exact: true })).toHaveAttribute("aria-current", "page");
        const dimensions = await session.page.evaluate(() => ({
          clientWidth: document.documentElement.clientWidth,
          scrollWidth: document.documentElement.scrollWidth,
        }));
        expect(dimensions.scrollWidth, `${route.path} must not overflow the narrow viewport`).toBeLessThanOrEqual(dimensions.clientWidth);
      });
    }
  } finally {
    await closeGuardedPage(session);
  }
});

test("page headers omit superheaders across user and administrator workspaces", async ({ browser }) => {
  const session = await guardedPage(browser, { session: administratorSession });
  try {
    for (const route of presentationRoutes) {
      await test.step(route.path, async () => {
        await session.page.goto(`${origin}${route.path}`);
        await expect(session.page.locator("main header:has(#page-title) p")).toHaveCount(1);
      });
    }
  } finally {
    await closeGuardedPage(session);
  }
});

test("empty entity collections show only No data", async ({ browser }) => {
  const session = await guardedPage(browser, { emptyCollections: true, session: administratorSession });
  try {
    for (const path of [
      "/envelopes",
      "/envelopes/new",
      `/envelopes/${envelopeId}/runs`,
      "/runs",
      `/runs/${taskUid}`,
      "/admin/runs",
      `/admin/runs/${taskUid}`,
      "/admin/envelopes/templates",
      "/admin/workflows",
      "/admin/approvals",
    ]) {
      await test.step(path, async () => {
        await session.page.goto(`${origin}${path}`);
        const emptyState = session.page.getByRole("heading", { name: "No data", exact: true });
        await expect(emptyState).toBeVisible();
        expect(await emptyState.locator("..").innerText()).toBe("No data");
      });
    }
  } finally {
    await closeGuardedPage(session);
  }
});

test("typed browser APIs drive envelope, run, connection, and administrator views", async ({ browser }) => {
  const developer = await guardedPage(browser);
  try {
    await developer.page.goto(`${origin}/envelopes`);
    await expect(developer.page.getByRole("heading", { name: "developer" })).toBeVisible();
    await expect(developer.page.getByText("25.00 USD")).toBeVisible();

    await developer.page.goto(`${origin}/envelopes/new`);
    const template = developer.page.getByLabel("Template");
    await expect(template.locator("option")).toHaveText(["Analyst · revision 2", "Developer · revision 4"]);
    await expect(template).toHaveValue("analyst");
    await expect(developer.page.getByLabel("Monthly limit (USD)")).toHaveValue("10.00");
    await expect(developer.page.getByLabel("Time to live")).toHaveValue("2h");
    await template.selectOption("developer");
    await expect(developer.page.getByLabel("Monthly limit (USD)")).toHaveValue("25.00");
    await expect(developer.page.getByLabel("Time to live")).toHaveValue("4h");
    await developer.page.getByRole("button", { name: "Submit request" }).click();
    await expect(developer.page).toHaveURL(`${origin}/envelopes/${envelopeId}`);
    const provisioned = developer.page.getByText("provisioned", { exact: true });
    await expect(provisioned).toHaveCSS("background-color", "rgb(18, 53, 36)");
    await expect(provisioned).toHaveCSS("border-color", "rgb(47, 128, 85)");
    await expect(provisioned).toHaveCSS("color", "rgb(134, 239, 172)");
    const envelopeMutation = developer.mutations.find((mutation) => mutation.path === "/app/api/v1/envelope-requests");
    expectMutationProof(envelopeMutation);
    expect(envelopeMutation.body.requestedEnvelope.spec.budget.singleRunLimit).toBe("5.00");

    const workflow = developer.page.getByRole("combobox", { name: "Workflow", exact: true });
    await expect(workflow.locator("option")).toHaveText(["Repository review · repository-review@1"]);
    await expect(workflow).toHaveValue("repository-review@1");
    await developer.page.getByRole("button", { name: "Render workflow" }).click();
    await expect(developer.page.getByLabel("Generated workflow")).toContainText("Steward governed run");
    await expect(developer.page.getByLabel("Generated workflow")).toContainText("workflow: repository-review@1");
    const workflowMutation = developer.mutations.find((mutation) => mutation.path.endsWith("/github-actions-workflow"));
    expectMutationProof(workflowMutation);
    expect(workflowMutation.body).toEqual({ workflow: "repository-review@1" });

    await developer.page.goto(`${origin}/envelopes/${envelopeId}/runs`);
    await expect(developer.page.getByText("repository-review@1")).toBeVisible();

    await developer.page.goto(`${origin}/runs`);
    await expect(developer.page.getByText("repository-review@1")).toBeVisible();

    await developer.page.goto(`${origin}/runs/${taskUid}`);
    await expect(developer.page.getByRole("heading", { name: "repository-review@1" })).toBeVisible();
    await expect(developer.page.getByText("1.25 USD")).toBeVisible();
    await expect(developer.page.getByRole("heading", { name: "Timeline" })).toBeVisible();

    await developer.page.goto(`${origin}/connections`);
    await expect(developer.page.getByRole("heading", { name: "GitHub" })).toBeVisible();
    await expect(developer.page.getByText("alice@example.com").last()).toBeVisible();
    await developer.page.getByRole("checkbox", { name: "I understand this revokes the Steward connection." }).check();
    await developer.page.getByRole("button", { name: "Disconnect GitHub" }).click();
    await expect.poll(() => developer.mutations.some((mutation) => mutation.path.endsWith("/disconnect"))).toBe(true);
    expectMutationProof(developer.mutations.find((mutation) => mutation.path.endsWith("/disconnect")));

    await developer.page.goto(`${origin}/settings`);
    await expect(developer.page.getByRole("heading", { name: "Server-owned session" })).toBeVisible();
  } finally {
    await closeGuardedPage(developer);
  }

  const administrator = await guardedPage(browser, { session: { ...developerSession, role: "admin", memberRoles: ["admin", "developer"] } });
  try {
    await administrator.page.goto(`${origin}/admin/runs`);
    await expect(administrator.page.getByRole("heading", { name: "All runs" })).toBeVisible();
    await expect(administrator.page.getByText("repository-review@1")).toBeVisible();
    await administrator.page.goto(`${origin}/admin/runs/${taskUid}`);
    await expect(administrator.page.getByRole("heading", { name: "Timeline" })).toBeVisible();
  } finally {
    await closeGuardedPage(administrator);
  }
});

test("Run detail displays the exact pinned Workflow and User Envelope revision", async ({ browser }) => {
  const developer = await guardedPage(browser);
  try {
    await developer.page.goto(`${origin}/runs/${taskUid}`);
    await expect(developer.page.getByRole("heading", { name: "repository-review@1" })).toBeVisible();
    await expect(developer.page.getByText("Workflow version", { exact: true })).toBeVisible();
    await expect(developer.page.getByText("repository-review@1", { exact: true }).last()).toBeVisible();
    const envelopeRevision = developer.page.getByText("User envelope revision", { exact: true }).locator("..");
    await expect(envelopeRevision).toContainText("4");
  } finally {
    await closeGuardedPage(developer);
  }
});

test("administrator publishes immutable Workflow revisions through the browser contract", async ({ browser }) => {
  const administrator = await guardedPage(browser, { session: administratorSession });
  try {
    await administrator.page.goto(`${origin}/admin/workflows`);
    await expect(administrator.page.getByRole("heading", { name: "Repository review", exact: true })).toBeVisible();
    await expect(administrator.page.getByText("repository-review@1", { exact: true })).toBeVisible();
    await administrator.page.getByRole("link", { name: "View version" }).click();
    await expect(administrator.page).toHaveURL(`${origin}/admin/workflows/repository-review/versions/1`);
    await expect(administrator.page.getByText(workflowRevision.prompt, { exact: true })).toBeVisible();

    await administrator.page.goto(`${origin}/admin/workflows/new`);
    await administrator.page.getByRole("textbox", { name: "Name", exact: true }).fill("repository-analysis");
    await administrator.page.getByLabel("Display name").fill("Repository analysis");
    await expect(administrator.page.getByLabel("Agent")).toHaveValue("codex@0.117.0");
    await administrator.page.getByLabel("Prompt").fill("Analyze the repository state.");
    await administrator.page.getByRole("button", { name: "Publish workflow" }).click();
    await expect(administrator.page).toHaveURL(`${origin}/admin/workflows/repository-analysis/versions/1`);
    const initial = administrator.mutations.find((mutation) => mutation.path === "/admin/api/v1/workflows");
    expectMutationProof(initial);
    expect(initial.body).toEqual({
      agent: "codex@0.117.0",
      displayName: "Repository analysis",
      name: "repository-analysis",
      prompt: "Analyze the repository state.",
    });

    await administrator.page.goto(`${origin}/admin/workflows/repository-review/new-version`);
    await expect(administrator.page.getByRole("textbox", { name: "Name", exact: true })).toBeDisabled();
    await administrator.page.getByLabel("Prompt").fill("Review the repository state again.");
    await administrator.page.getByRole("button", { name: "Publish new version" }).click();
    await expect(administrator.page).toHaveURL(`${origin}/admin/workflows/repository-review/versions/2`);
    const next = administrator.mutations.find((mutation) => mutation.path.endsWith("/repository-review/versions"));
    expectMutationProof(next);
    expect(next.body).toEqual({
      agent: "codex@0.117.0",
      displayName: "Repository review",
      prompt: "Review the repository state again.",
    });
  } finally {
    await closeGuardedPage(administrator);
  }
});

test("administrator templates and approvals use typed browser authority", async ({ browser }) => {
  const administrator = await guardedPage(browser, { session: administratorSession });
  try {
    await administrator.page.goto(`${origin}/admin/envelopes/templates`);
    await expect(administrator.page.getByRole("link", { name: /Analyst/ })).toBeVisible();
    await expect(administrator.page.getByRole("link", { name: /Developer/ })).toBeVisible();
    await administrator.page.getByRole("link", { name: /Analyst/ }).click();
    await expect(administrator.page).toHaveURL(`${origin}/admin/envelopes/templates/analyst`);
    await expect(administrator.page.getByText("Current revision 4")).toBeVisible();
    const limitType = administrator.page.getByRole("combobox", { name: "Limit type" });
    const limitAmount = administrator.page.getByRole("textbox", { name: "Limit amount (USD)" });
    await expect(limitType.locator("option")).toHaveText(["Single run", "Monthly"]);
    await expect(limitAmount).toHaveValue("2.50");
    await limitType.selectOption("monthly");
    await expect(limitAmount).toHaveValue("25.00");
    await limitAmount.fill("30.00");
    await limitType.selectOption("singleRun");
    await limitAmount.fill("3.00");
    await expect(administrator.page.getByRole("textbox", { name: "TTL" })).toHaveValue("4h");
    await expect(administrator.page.getByRole("combobox", { name: "Currency" })).toHaveValue("USD");
    await expect(administrator.page.getByRole("combobox", { name: "Currency" }).locator("option")).toHaveText(["USD"]);
    await expect(administrator.page.getByRole("group", { name: "Models" }).getByRole("listitem")).toHaveText(["openai/gpt-5.4"]);
    const model = administrator.page.getByRole("combobox", { name: "Model" });
    await expect(model).toHaveValue("openai/gpt-5.4");
    await expect(model.locator("option")).toHaveText([
      "openai/gpt-5.4",
      "openai/gpt-5.3",
      "anthropic/claude-opus-4",
      "google/gemini-2.5-pro",
    ]);
    await expect(model.locator("option").nth(0)).toBeEnabled();
    for (const option of await model.locator("option").all().then((options) => options.slice(1))) await expect(option).toHaveAttribute("disabled", "");
    await expect(administrator.page.getByText("Only GPT-5.4 is currently available. Other models are disabled.", { exact: true })).toHaveCount(0);

    const toolProvider = administrator.page.getByRole("combobox", { name: "Tool provider" });
    await expect(toolProvider).toHaveValue("github");
    await expect(toolProvider.locator("option")).toHaveText(["GitHub", "GitLab", "Jira"]);
    await expect(toolProvider.locator("option").nth(0)).toBeEnabled();
    await expect(toolProvider.locator("option").nth(1)).toHaveAttribute("disabled", "");
    await expect(toolProvider.locator("option").nth(2)).toHaveAttribute("disabled", "");

    const tool = administrator.page.getByRole("combobox", { exact: true, name: "Tool" });
    await expect(tool).toHaveValue("repository:get_file_contents");
    await expect(tool.locator("option")).toHaveText([
      "repository:get_file_contents",
      "repository:list_issues",
      "repository:create_issue",
      "pull_request:get",
    ]);
    await expect(tool.locator("option").nth(0)).toBeEnabled();
    for (const option of await tool.locator("option").all().then((options) => options.slice(1))) await expect(option).toHaveAttribute("disabled", "");
    const advanced = administrator.page.getByText("Advanced", { exact: true }).locator("..");
    await expect(advanced).not.toHaveAttribute("open", "");
    await administrator.page.getByRole("button", { name: "Save new version" }).click();
    await expect(administrator.page.getByText("Template revision accepted by the Rust authority.")).toBeVisible();
    await expect(administrator.page.getByText("Current revision 5")).toBeVisible();
    const templateMutation = administrator.mutations.find((mutation) => mutation.path === "/admin/api/v1/envelope-templates/analyst");
    expectMutationProof(templateMutation);
    expect(templateMutation.body.revision).toBe(5);
    expect(templateMutation.body.spec.budget).toEqual({
      currency: "USD",
      monthlyLimit: "30.00",
      singleRunLimit: "3.00",
    });
    await administrator.page.getByRole("textbox", { name: "New template ID" }).fill("reviewer");
    await administrator.page.getByRole("button", { name: "Save as new" }).click();
    await expect.poll(() => administrator.mutations.some((mutation) => mutation.path === "/admin/api/v1/envelope-templates/reviewer")).toBe(true);
    const copiedTemplate = administrator.mutations.find((mutation) => mutation.path === "/admin/api/v1/envelope-templates/reviewer");
    expectMutationProof(copiedTemplate);
    expect(copiedTemplate.body.revision).toBe(1);

    await administrator.page.goto(`${origin}/admin/approvals`);
    await expect(administrator.page.getByRole("heading", { name: "Pending approvals" })).toBeVisible();
    const envelopeRequestCard = administrator.page.getByRole("listitem").filter({
      has: administrator.page.getByRole("heading", { name: "User envelope request" }),
    });
    await expect(envelopeRequestCard).toBeVisible();
    await expect(envelopeRequestCard.getByText("alice@example.com")).toBeVisible();
    await expect(envelopeRequestCard.getByText("developer", { exact: true })).toBeVisible();
    await expect(administrator.page.getByText("runtime-example-2")).toBeVisible();
    await administrator.page.getByRole("button", { name: "File decision reference" }).click();
    await expect(administrator.page.getByText("Decision reference filed through the server-owned channel.")).toBeVisible();
    expectMutationProof(administrator.mutations.find((mutation) => mutation.path.endsWith("/file")));
    await administrator.page.getByLabel("Rationale").fill("Approved for one bounded investigation.");
    await administrator.page.getByLabel("Expires at (RFC 3339)").fill("2026-08-25T17:00:00Z");
    await administrator.page.getByRole("button", { name: "Approve exception" }).click();
    await expect(administrator.page.getByText("Approval applied through the governed Rust admission path.")).toBeVisible();
    const approvalMutation = administrator.mutations.find((mutation) => mutation.path.endsWith("/approve"));
    expectMutationProof(approvalMutation);
    expect(approvalMutation.body.evidenceUrl).toBe("https://example.com/decisions/PROJ-123");

    await administrator.page.goto(`${origin}/admin/settings`);
    await expect(administrator.page.getByRole("heading", { name: "Administrator session" })).toBeVisible();
  } finally {
    await closeGuardedPage(administrator);
  }
});

test("an incomplete successful template response fails closed instead of crashing", async ({ browser }) => {
  const administrator = await guardedPage(browser, {
    malformedAdminTemplate: true,
    session: administratorSession,
  });
  try {
    await administrator.page.goto(`${origin}/admin/envelopes/templates/analyst`);
    await expect(administrator.page.getByRole("heading", { name: "Data could not be accepted" })).toBeVisible();
  } finally {
    await closeGuardedPage(administrator);
  }
});

test("the deployed admin template contract is accepted during the rolling Next cutover", async ({ browser }) => {
  const administrator = await guardedPage(browser, {
    legacyAdminTemplate: true,
    session: administratorSession,
  });
  try {
    await administrator.page.goto(`${origin}/admin/envelopes/templates/analyst`);
    await expect(administrator.page.getByText("Current revision 4")).toBeVisible();
    await administrator.page.getByRole("combobox", { name: "Limit type" }).selectOption("monthly");
    await expect(administrator.page.getByRole("textbox", { name: "Limit amount (USD)" })).toHaveValue("25.00");
    await expect(administrator.page.getByText("provider-a/model-a", { exact: true })).toBeVisible();
  } finally {
    await closeGuardedPage(administrator);
  }
});

test("connection OAuth starts through a same-origin Rust mutation", async ({ browser }) => {
  const developer = await guardedPage(browser, { connectionPhase: "disconnected" });
  try {
    await developer.page.goto(`${origin}/connections`);
    await developer.page.getByRole("button", { name: "Connect GitHub" }).click();
    await expect(developer.page).toHaveURL(`${origin}/connections?oauth=started`);
    expectMutationProof(developer.mutations.find((mutation) => mutation.path.endsWith("/start")));
  } finally {
    await closeGuardedPage(developer);
  }
});

test("envelope request keeps a Rust authorization denial explicit", async ({ browser }) => {
  const developer = await guardedPage(browser, {
    expectedHttpStatuses: [403],
    mutationFailures: { "/app/api/v1/envelope-requests": 403 },
  });
  try {
    await developer.page.goto(`${origin}/envelopes/new`);
    await developer.page.getByRole("button", { name: "Submit request" }).click();
    await expect(developer.page.getByText("The Rust authorization boundary rejected the request.")).toBeVisible();
  } finally {
    await closeGuardedPage(developer);
  }
});

test("workflow rendering keeps an authoritative outage explicit", async ({ browser }) => {
  const workflowPath = `/app/api/v1/envelope-requests/${envelopeId}/github-actions-workflow`;
  const developer = await guardedPage(browser, {
    expectedHttpStatuses: [503],
    mutationFailures: { [workflowPath]: 503 },
  });
  try {
    await developer.page.goto(`${origin}/envelopes/${envelopeId}`);
    await expect(developer.page.getByRole("combobox", { name: "Workflow", exact: true })).toHaveValue("repository-review@1");
    await developer.page.getByRole("button", { name: "Render workflow" }).click();
    await expect(developer.page.getByText("The authoritative workflow service is unavailable.")).toBeVisible();
  } finally {
    await closeGuardedPage(developer);
  }
});

test("connection action keeps a Rust authorization denial explicit", async ({ browser }) => {
  const developer = await guardedPage(browser, {
    connectionPhase: "disconnected",
    expectedHttpStatuses: [403],
    mutationFailures: { "/admin/api/v1/connections/github/start": 403 },
  });
  try {
    await developer.page.goto(`${origin}/connections`);
    await developer.page.getByRole("button", { name: "Connect GitHub" }).click();
    await expect(developer.page.getByText("The Rust authorization boundary rejected the connection action.")).toBeVisible();
  } finally {
    await closeGuardedPage(developer);
  }
});
