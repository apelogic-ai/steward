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

const envelope = {
  revision: 4,
  spec: {
    budget: { currency: "USD", monthlyLimit: "25.00" },
    llms: [{ provider: "provider-a", model: "model-a" }],
    tools: [{ provider: "github", resource: "repository", action: "get_file_contents" }],
    ttl: "4h",
    runner: { platforms: [] },
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

const run = {
  taskUid,
  workflow: "code-review",
  codingAgentRuntime: "agent-v1",
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
        || requestUrl.pathname === `/admin/api/v1/approvals/${approvalId}/approve`
        || requestUrl.pathname === `/admin/api/v1/approvals/${approvalId}/file`
        || requestUrl.pathname === "/admin/api/v1/connections/github/start"
        || requestUrl.pathname === "/admin/api/v1/connections/github/disconnect"
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
          response.writeHead(201, { "content-type": "application/json", "cache-control": "no-store" });
          response.end(JSON.stringify({ apiVersion: "steward.browser-admin/v1", memberRole: "analyst", envelope: JSON.parse(rawBody) }));
          return;
        }
        if (requestUrl.pathname.endsWith("/github-actions-workflow")) {
          response.writeHead(200, { "content-type": "application/json", "cache-control": "no-store" });
          response.end(JSON.stringify({ apiVersion: "steward.envelope-requests/v1", workflow: { schemaVersion: "v1", contentType: "application/yaml", sha256: "abc123", yaml: "name: Steward governed run\n" } }));
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
  connectionPhase = "connected",
  expectedHttpStatuses = [],
  mutationFailures = {},
  session = developerSession,
  viewport = { width: 1280, height: 800 },
} = {}) {
  const context = await browser.newContext({ viewport });
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
  const json = (route, body, status = 200) => route.fulfill({ status, contentType: "application/json", body: JSON.stringify(body) });
  await context.route(`${origin}/app/api/v1/envelope-templates`, (route) => json(route, {
    apiVersion: "steward.envelope-requests/v1",
    templates: [{ id: "developer", displayName: "Developer", revision: 4, ceiling: envelope, autoProvisionThreshold: null, githubConnection: "connected" }],
  }));
  await context.route(`${origin}/app/api/v1/envelope-requests`, async (route) => {
    if (route.request().method() === "POST") {
      await route.continue();
      return;
    }
    await json(route, { apiVersion: "steward.envelope-requests/v1", requests: [envelopeRequest] });
  });
  await context.route(`${origin}/app/api/v1/envelope-requests/**`, async (route) => {
    if (route.request().url().endsWith("/github-actions-workflow")) {
      await route.continue();
      return;
    }
    await json(route, { apiVersion: "steward.envelope-requests/v1", request: envelopeRequest });
  });
  await context.route(`${origin}/app/api/v1/runs*`, (route) => json(route, { apiVersion: "steward.browser-runs/v1", runs: [run], nextCursor: null }));
  await context.route(`${origin}/app/api/v1/runs/**`, (route) => route.request().url().endsWith("/timeline")
    ? json(route, { apiVersion: "steward.browser-runs/v1", taskUid, events: [{ kind: "phase", phase: "succeeded", at: "2026-08-24T17:03:00Z" }] })
    : json(route, { apiVersion: "steward.browser-runs/v1", run }));
  await context.route(`${origin}/admin/api/v1/all-runs*`, (route) => json(route, { apiVersion: "steward.browser-runs/v1", runs: [{ ...run, ownerUserId: developerSession.principal.userId }], nextCursor: null }));
  await context.route(`${origin}/admin/api/v1/all-runs/**`, (route) => route.request().url().endsWith("/timeline")
    ? json(route, { apiVersion: "steward.browser-runs/v1", taskUid, events: [{ kind: "phase", phase: "succeeded", at: "2026-08-24T17:03:00Z" }] })
    : json(route, { apiVersion: "steward.browser-runs/v1", run }));
  await context.route(`${origin}/admin/api/v1/envelope-templates/**`, async (route) => {
    if (route.request().method() === "POST") {
      await route.continue();
      return;
    }
    await json(route, { apiVersion: "steward.browser-admin/v1", memberRole: "analyst", envelope });
  });
  await context.route(`${origin}/admin/api/v1/approvals`, (route) => json(route, {
    apiVersion: "steward.browser-admin/v1",
    approvals: [approval],
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

    await expect(session.page.getByText("alice@example.com")).toBeVisible();
    await expect(session.page.getByRole("link", { name: "Envelopes", exact: true })).toHaveAttribute("aria-current", "page");
    await session.page.getByRole("link", { name: "Runs", exact: true }).click();
    await expect(session.page).toHaveURL(`${origin}/runs`);
    await expect(session.page.getByRole("link", { name: "Runs", exact: true })).toHaveAttribute("aria-current", "page");
  } finally {
    await closeGuardedPage(session);
  }
});

test("every presentation route has an explicit unauthorized state", async ({ browser }) => {
  const unauthorized = await guardedPage(browser, { session: null });
  try {
    for (const route of presentationRoutes) {
      await test.step(route.path, async () => {
        await unauthorized.page.goto(`${origin}${route.path}`);
        await expect(unauthorized.page.getByRole("heading", { name: "Sign in required" })).toBeVisible();
        await expect(unauthorized.page.getByRole("link", { name: "Continue with Google" })).toBeVisible();
      });
    }
  } finally {
    await closeGuardedPage(unauthorized);
  }
});

test("developer-forbidden and server-declared dual-role modes stay explicit", async ({ browser }) => {
  const developer = await guardedPage(browser);
  try {
    await developer.page.goto(`${origin}/admin/runs`);
    await expect(developer.page.getByRole("heading", { name: "Forbidden" })).toBeVisible();
    await expect(developer.page.getByLabel("Workspace view")).toHaveCount(0);
  } finally {
    await closeGuardedPage(developer);
  }

  const dualRole = await guardedPage(browser, { session: administratorSession });
  try {
    await dualRole.page.goto(`${origin}/envelopes`);
    await dualRole.page.getByLabel("Workspace view").selectOption("admin");
    await expect(dualRole.page).toHaveURL(`${origin}/admin/runs`);
    await expect(dualRole.page.getByRole("link", { name: "Runs", exact: true })).toHaveAttribute("aria-current", "page");
  } finally {
    await closeGuardedPage(dualRole);
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

test("typed browser APIs drive envelope, run, connection, and administrator views", async ({ browser }) => {
  const developer = await guardedPage(browser);
  try {
    await developer.page.goto(`${origin}/envelopes`);
    await expect(developer.page.getByRole("heading", { name: "developer" })).toBeVisible();
    await expect(developer.page.getByText("25.00 USD")).toBeVisible();

    await developer.page.goto(`${origin}/envelopes/new`);
    await expect(developer.page.getByLabel("Template")).toHaveValue("developer");
    await developer.page.getByRole("button", { name: "Submit request" }).click();
    await expect(developer.page).toHaveURL(`${origin}/envelopes/${envelopeId}`);
    expectMutationProof(developer.mutations.find((mutation) => mutation.path === "/app/api/v1/envelope-requests"));

    await developer.page.getByLabel("Repository").fill("example-org/repository");
    await developer.page.getByLabel("Revision").fill("0123456789abcdef0123456789abcdef01234567");
    await developer.page.getByLabel("Path").fill("src/lib.rs");
    await developer.page.getByRole("button", { name: "Render workflow" }).click();
    await expect(developer.page.getByLabel("Generated workflow")).toContainText("Steward governed run");
    expectMutationProof(developer.mutations.find((mutation) => mutation.path.endsWith("/github-actions-workflow")));

    await developer.page.goto(`${origin}/envelopes/${envelopeId}/runs`);
    await expect(developer.page.getByText("code-review")).toBeVisible();

    await developer.page.goto(`${origin}/runs`);
    await expect(developer.page.getByText("code-review")).toBeVisible();

    await developer.page.goto(`${origin}/runs/${taskUid}`);
    await expect(developer.page.getByRole("heading", { name: "code-review" })).toBeVisible();
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
    await expect(administrator.page.getByText("code-review")).toBeVisible();
    await administrator.page.goto(`${origin}/admin/runs/${taskUid}`);
    await expect(administrator.page.getByRole("heading", { name: "Timeline" })).toBeVisible();
  } finally {
    await closeGuardedPage(administrator);
  }
});

test("administrator templates and approvals use typed browser authority", async ({ browser }) => {
  const administrator = await guardedPage(browser, { session: administratorSession });
  try {
    await administrator.page.goto(`${origin}/admin/envelopes/templates`);
    await expect(administrator.page.getByRole("textbox", { name: "Template role" })).toHaveValue("analyst");
    await expect(administrator.page.getByRole("spinbutton", { name: "Revision" })).toHaveValue("4");
    await administrator.page.getByRole("spinbutton", { name: "Revision" }).fill("5");
    await administrator.page.getByRole("button", { name: "Author template revision" }).click();
    await expect(administrator.page.getByText("Template revision accepted by the Rust authority.")).toBeVisible();
    await expect(administrator.page.getByText("Current revision 5")).toBeVisible();
    const templateMutation = administrator.mutations.find((mutation) => mutation.path === "/admin/api/v1/envelope-templates/analyst");
    expectMutationProof(templateMutation);
    expect(templateMutation.body.revision).toBe(5);

    await administrator.page.goto(`${origin}/admin/approvals`);
    await expect(administrator.page.getByRole("heading", { name: "Pending approvals" })).toBeVisible();
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
    await developer.page.getByLabel("Repository").fill("example-org/repository");
    await developer.page.getByLabel("Revision").fill("0123456789abcdef0123456789abcdef01234567");
    await developer.page.getByLabel("Path").fill("src/lib.rs");
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
