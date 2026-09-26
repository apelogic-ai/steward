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
const rerunTaskUid = "00000000-0000-0000-0000-000000000006";
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
    llms: [{ provider: "provider-a", model: "model-a" }],
  },
};

const capabilityCatalog = {
  schemaVersion: "steward.capability-catalog/v2",
  models: [
    { provider: "provider-a", model: "model-a" },
    { provider: "provider-b", model: "model-b" },
  ],
  tools: envelope.spec.tools.map((tool) => ({ ...tool, accessClass: "read" })),
  catalogs: [{ provider: "github", catalogId: "github-tools", version: "1.6.0", available: true }],
};

const githubReadTools = [
  "actions_get", "actions_list", "get_job_logs", "get_file_contents", "list_commits",
  "get_commit", "get_release", "list_releases", "get_workflow", "list_workflows",
].map((resource) => ({ provider: "github", resource, action: "read", accessClass: "read" }));

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
  history: [{ status: "provisioned", at: "2026-08-24T17:01:00Z", actor: "system:auto", reason: null }],
};

const pendingEnvelopeRequest = {
  requestId: "00000000-0000-0000-0000-000000000004",
  ownerUserId: developerSession.principal.userId,
  ownerDisplayEmail: developerSession.principal.displayEmail,
  templateId: "developer",
  templateRevision: 4,
  requestedEnvelope: envelope,
  templateEnvelope: adminEnvelope,
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
  codingAgentRuntime: "codex@0.140.0",
  runtimeUid: "runtime-example-1",
  runtimeOwnership: "provisioned",
  phase: "succeeded",
  finalizationRequested: true,
  finalized: true,
  createdAt: "2026-08-24T17:02:00Z",
  updatedAt: "2026-08-24T17:03:00Z",
  observedSpend: { observedAmount: "1.25", currency: "USD", exhausted: false },
  errorCategory: null,
  stages: [
    { id: "admission", displayName: "Admission", state: "succeeded", steps: [] },
    { id: "provision_runtime", displayName: "Provision runtime", state: "succeeded", steps: [] },
    { id: "agent_execution", displayName: "Agent execution", state: "succeeded", steps: [{ id: "execution", displayName: "Execution", state: "succeeded", logStreams: ["stdout", "stderr"] }] },
    { id: "finalize", displayName: "Finalize", state: "succeeded", steps: [] },
  ],
};

const runFacets = {
  submitted: 0,
  queued: 0,
  running: 0,
  parked: 0,
  succeeded: 1,
  failed: 0,
  cancelled: 0,
};

const unifiedEnvelopeRequest = {
  id: pendingEnvelopeRequest.requestId,
  kind: "ceiling_exceeded",
  source: "envelope_request",
  state: "requested",
  requester: { userId: pendingEnvelopeRequest.ownerUserId, displayEmail: pendingEnvelopeRequest.ownerDisplayEmail },
  template: { id: pendingEnvelopeRequest.templateId, displayName: "Developer", revision: pendingEnvelopeRequest.templateRevision },
  createdAt: pendingEnvelopeRequest.createdAt,
  stateAt: pendingEnvelopeRequest.createdAt,
  stateActor: pendingEnvelopeRequest.ownerUserId,
  deltas: [{ dimension: "budget", requested: "40.00", ceiling: "25.00", currency: "USD" }],
  requestedEnvelope: pendingEnvelopeRequest.requestedEnvelope,
  templateEnvelope: pendingEnvelopeRequest.templateEnvelope,
  history: [{ state: "requested", at: pendingEnvelopeRequest.createdAt, actor: pendingEnvelopeRequest.ownerUserId, reason: null }],
};

const unifiedRuntimeApproval = {
  id: approvalId,
  kind: "ceiling_exceeded",
  source: "runtime_exception",
  state: "requested",
  requester: { userId: developerSession.principal.userId, displayEmail: "alice@example.com" },
  template: { id: "analyst", displayName: "Analyst", revision: 4 },
  createdAt: "2026-08-24T17:05:00Z",
  stateAt: "2026-08-24T17:05:00Z",
  stateActor: developerSession.principal.userId,
  deltas: [{ dimension: "budget", requested: "40.00", ceiling: "25.00", currency: "USD" }],
  history: [{ state: "requested", at: "2026-08-24T17:05:00Z", actor: developerSession.principal.userId, reason: null }],
};

const workflowRevision = {
  name: "repository-review",
  version: 1,
  displayName: "Repository review",
  agent: "codex@0.140.0",
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
  { path: "/admin/approvals", heading: "Requests", activeNavigation: "Approvals" },
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
  let rerunFixtures = [];
  let rerunFixtureIndex = 0;
  const proxy = createServer(async (request, response) => {
    try {
      const requestUrl = new URL(request.url ?? "/", nextOrigin);
      const templateMutation = request.method === "PUT"
        && requestUrl.pathname.startsWith("/admin/api/v1/envelope-templates/");
      const browserMutation = templateMutation || (request.method === "POST" && (
        requestUrl.pathname === "/app/api/v1/envelope-requests"
        || requestUrl.pathname.endsWith("/github-actions-workflow")
        || requestUrl.pathname.startsWith("/admin/api/v1/envelope-templates/")
        || requestUrl.pathname === "/admin/api/v1/workflows"
        || requestUrl.pathname.endsWith("/versions")
        || requestUrl.pathname === `/admin/api/v1/envelope-requests/${pendingEnvelopeRequest.requestId}/approve`
        || requestUrl.pathname === `/admin/api/v1/envelope-requests/${pendingEnvelopeRequest.requestId}/reject`
        || requestUrl.pathname === `/admin/api/v1/approvals/${approvalId}/approve`
        || requestUrl.pathname === `/admin/api/v1/approvals/${approvalId}/file`
        || requestUrl.pathname === "/app/api/v1/connections/github/start"
        || requestUrl.pathname === "/app/api/v1/connections/github/disconnect"
        || requestUrl.pathname === "/admin/api/v1/connections/github/start"
        || requestUrl.pathname === "/admin/api/v1/connections/github/disconnect"
        || requestUrl.pathname.endsWith("/rerun")
        || requestUrl.pathname === "/admin/auth/logout"
      ));
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
        if (requestUrl.pathname.endsWith("/rerun")) {
          const fixture = rerunFixtures[Math.min(rerunFixtureIndex, rerunFixtures.length - 1)];
          rerunFixtureIndex += 1;
          response.writeHead(fixture?.status ?? 503, { "content-type": "application/json", "cache-control": "no-store" });
          response.end(JSON.stringify(fixture?.body ?? {}));
          return;
        }
        if (requestUrl.pathname.startsWith("/admin/api/v1/envelope-templates/")) {
          const templateId = decodeURIComponent(requestUrl.pathname.split("/").at(-1) ?? "");
          const submitted = JSON.parse(rawBody);
          response.writeHead(201, { "content-type": "application/json", "cache-control": "no-store" });
          response.end(JSON.stringify({
            apiVersion: "steward.browser-admin/v1",
            id: templateId,
            displayName: submitted.displayName,
            memberRoles: submitted.memberRoles,
            envelope: submitted.envelope,
            autoProvisionThreshold: submitted.autoProvisionThreshold ?? null,
          }));
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
          response.end(JSON.stringify({ apiVersion: "steward.envelope-requests/v1", workflow: { schemaVersion: "v2", contentType: "application/yaml", suggestedPath: ".github/workflows/steward-repository-review.yml", sha256: "abc123", yaml: ["name: Steward governed run", "on:", "  workflow_dispatch:", "jobs:", "  governed:", "    with:", "      workflow: repository-review@1", ""].join("\n") } }));
          return;
        }
        if (requestUrl.pathname.startsWith("/admin/api/v1/envelope-requests/")) {
          const provisioned = requestUrl.pathname.endsWith("/approve");
          response.writeHead(200, { "content-type": "application/json", "cache-control": "no-store" });
          response.end(JSON.stringify({
            apiVersion: "steward.browser-admin/v1",
            request: {
              actedBy: administratorSession.principal.userId,
              approvalId: provisioned ? "00000000-0000-0000-0000-000000000005" : null,
              approvedEnvelope: provisioned ? pendingEnvelopeRequest.requestedEnvelope : null,
              envelopeDigest: provisioned ? `sha256:${"d".repeat(64)}` : null,
              envelopeInstanceId: provisioned ? "env_00000000000000000000000000000004" : null,
              reason: provisioned ? null : rawBody ? JSON.parse(rawBody).reason : null,
              requestId: pendingEnvelopeRequest.requestId,
              requestedEnvelope: pendingEnvelopeRequest.requestedEnvelope,
              status: provisioned ? "provisioned" : "rejected",
              statusAt: "2026-08-24T17:06:00Z",
              templateId: pendingEnvelopeRequest.templateId,
              templateRevision: pendingEnvelopeRequest.templateRevision,
            },
          }));
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
    useRerunFixtures: (fixtures) => {
      rerunFixtures = fixtures;
      rerunFixtureIndex = 0;
    },
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
  adminTemplateModels = adminEnvelope.spec.llms,
  adminTemplateTools = adminEnvelope.spec.tools,
  legacyAdminTemplate = false,
  malformedAdminTemplate = false,
  mockSignIn = true,
  colorScheme = "dark",
  connectionPhase = "connected",
  emptyCollections = false,
  expectedHttpStatuses = [],
  executionLogs = {
    stdout: { body: "agent stdout\n", status: 200 },
    stderr: { body: "agent stderr\n", status: 200 },
  },
  includeSampleWorkflow = false,
  onboardingPagination = false,
  mutationFailures = {},
  rerunResponses = [
    { status: 201, body: { apiVersion: "steward.browser-runs/v1", taskUid: rerunTaskUid } },
  ],
  runPhase = "succeeded",
  capabilityCatalogModels = capabilityCatalog.models,
  capabilityCatalogTools = capabilityCatalog.tools,
  capabilityCatalogStatus = 200,
  session = developerSession,
  viewport = { width: 1280, height: 800 },
} = {}) {
  const context = await browser.newContext({ colorScheme, viewport });
  const executionLogRequests = [];
  const mutations = [];
  web.useMutationFailures(mutationFailures);
  web.useMutationSink(mutations);
  web.useRerunFixtures(rerunResponses);
  await context.addInitScript(() => {
    const allowedPreference = (key) => typeof key === "string"
      && key.startsWith("steward.ui.envelope-accordion.");
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
    workflows: emptyCollections ? [] : [
      {
        agent: workflowRevision.agent,
        displayName: workflowRevision.displayName,
        name: workflowRevision.name,
        sample: false,
        version: workflowRevision.version,
      },
      ...(includeSampleWorkflow ? [{
        agent: workflowRevision.agent,
        displayName: "Repository summary",
        name: "repo-summary",
        sample: true,
        version: 1,
      }] : []),
    ],
  }));
  await context.route(`${origin}/app/api/v1/envelope-requests*`, async (route) => {
    if (route.request().method() === "POST") {
      await route.continue();
      return;
    }
    if (onboardingPagination) {
      const cursor = new URL(route.request().url()).searchParams.get("cursor");
      await json(route, cursor
        ? { apiVersion: "steward.envelope-requests/v1", requests: [envelopeRequest], nextCursor: null }
        : {
            apiVersion: "steward.envelope-requests/v1",
            requests: [{ ...envelopeRequest, id: "00000000-0000-0000-0000-000000000006", envelopeInstanceId: "runtime-example-6" }],
            nextCursor: "00000000-0000-0000-0000-000000000006",
          });
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
  await context.route(`${origin}/app/api/v1/runs*`, (route) => {
    if (onboardingPagination) {
      const cursor = new URL(route.request().url()).searchParams.get("cursor");
      return json(route, {
        apiVersion: "steward.browser-runs/v1",
        runs: cursor ? [{
          ...run,
          taskUid: "00000000-0000-0000-0000-000000000007",
          workflow: "repo-summary@1",
          workflowName: "repo-summary",
          workflowVersion: 1,
          trigger: { provider: "github", repository: "https://github.com/example-org/sample" },
        }] : [run],
        nextCursor: cursor ? null : taskUid,
        facets: { phase: runFacets },
      });
    }
    return json(route, { apiVersion: "steward.browser-runs/v1", runs: emptyCollections ? [] : [run], nextCursor: null, facets: { phase: emptyCollections ? { ...runFacets, succeeded: 0 } : runFacets } });
  });
  await context.route(`${origin}/app/api/v1/runs/**`, (route) => {
    if (route.request().url().endsWith("/rerun")) return route.continue();
    return route.request().url().endsWith("/timeline")
      ? json(route, { apiVersion: "steward.browser-runs/v1", taskUid, events: emptyCollections ? [] : [{ kind: "phase", phase: runPhase, at: "2026-08-24T17:03:00Z" }] })
      : json(route, { apiVersion: "steward.browser-runs/v1", run: { ...run, phase: runPhase } });
  });
  await context.route(`${origin}/app/api/v1/runs/${taskUid}/logs/*`, async (route) => {
    const stream = new URL(route.request().url()).pathname.split("/").at(-1);
    executionLogRequests.push(route.request());
    const fixture = executionLogs[stream];
    const content = fixture?.body ?? "";
    await route.fulfill({
      status: fixture?.status ?? 404,
      contentType: "application/json",
      body: fixture?.status === 200 ? JSON.stringify({ stream, content, truncated: false, sizeBytes: Buffer.byteLength(content), complete: true }) : "",
      headers: { "cache-control": "no-store" },
    });
  });
  await context.route(`${origin}/admin/api/v1/all-runs*`, (route) => json(route, { apiVersion: "steward.browser-runs/v1", runs: emptyCollections ? [] : [{ ...run, ownerUserId: developerSession.principal.userId, ownerDisplayEmail: developerSession.principal.displayEmail }], nextCursor: null, facets: { phase: emptyCollections ? { ...runFacets, succeeded: 0 } : runFacets } }));
  await context.route(`${origin}/admin/api/v1/all-runs/**`, (route) => route.request().url().endsWith("/timeline")
    ? json(route, { apiVersion: "steward.browser-runs/v1", taskUid, events: emptyCollections ? [] : [{ kind: "phase", phase: runPhase, at: "2026-08-24T17:03:00Z" }] })
    : json(route, { apiVersion: "steward.browser-runs/v1", run: { ...run, phase: runPhase } }));
  await context.route(`${origin}/admin/api/v1/all-runs/${taskUid}/logs/*`, async (route) => {
    const stream = new URL(route.request().url()).pathname.split("/").at(-1);
    executionLogRequests.push(route.request());
    const fixture = executionLogs[stream];
    const content = fixture?.body ?? "";
    await route.fulfill({
      status: fixture?.status ?? 404,
      contentType: "application/json",
      body: fixture?.status === 200 ? JSON.stringify({ stream, content, truncated: false, sizeBytes: Buffer.byteLength(content), complete: true }) : "",
      headers: { "cache-control": "no-store" },
    });
  });
  await context.route(`${origin}/admin/api/v1/envelope-templates/**`, async (route) => {
    if (route.request().method() === "POST" || route.request().method() === "PUT") {
      await route.continue();
      return;
    }
    const memberRole = decodeURIComponent(new URL(route.request().url()).pathname.split("/").at(-1) ?? "");
    await json(route, malformedAdminTemplate
      ? { apiVersion: "steward.browser-admin/v1", memberRole }
      : legacyAdminTemplate
        ? { apiVersion: "steward.admin/v1", template: { id: memberRole, revision: 4, envelope } }
        : { apiVersion: "steward.browser-admin/v1", id: memberRole, displayName: `${memberRole.charAt(0).toUpperCase()}${memberRole.slice(1)}`, memberRoles: [memberRole], envelope: {
          ...adminEnvelope,
          spec: { ...adminEnvelope.spec, llms: adminTemplateModels, tools: adminTemplateTools },
        } });
  });
  await context.route(`${origin}/admin/api/v1/envelope-templates`, (route) => json(route, {
    apiVersion: "steward.browser-admin/v1",
    templates: emptyCollections ? [] : [
      { id: "analyst", displayName: "Analyst", memberRoles: ["analyst"], envelope: adminEnvelope, autoProvisionThreshold: null },
      { id: "developer", displayName: "Developer", memberRoles: ["developer"], envelope, autoProvisionThreshold: null },
    ],
  }));
  await context.route(`${origin}/admin/api/v1/capabilities`, (route) => capabilityCatalogStatus === 200
    ? json(route, {
      schemaVersion: "steward.capability-catalog/v2",
      models: capabilityCatalogModels,
      tools: capabilityCatalogTools,
      catalogs: capabilityCatalog.catalogs,
    })
    : route.fulfill({ status: capabilityCatalogStatus, body: "" }));
  await context.route(`${origin}/admin/api/v1/workflows`, async (route) => {
    if (route.request().method() === "POST") {
      await route.continue();
      return;
    }
    await json(route, {
      apiVersion: "steward.workflows/v1",
      agents: [{ agentRef: workflowRevision.agent, displayName: "Codex" }],
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
  await context.route(`${origin}/admin/api/v1/requests*`, (route) => json(route, {
    apiVersion: "steward.browser-admin/v1",
    requests: emptyCollections ? [] : [unifiedEnvelopeRequest, unifiedRuntimeApproval],
    nextCursor: null,
  }));
  await context.route(`${origin}/admin/api/v1/requests/summary`, (route) => json(route, {
    apiVersion: "steward.browser-admin/v1",
    needsAction: emptyCollections ? 0 : 2,
    escalated: 0,
    requested: emptyCollections ? 0 : 2,
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
  await context.route(`${origin}/app/api/v1/connections`, (route) => json(route, {
    apiVersion: "steward.connections/v1",
    connections: [{
      provider: "github",
      displayName: "GitHub",
      status: connectionPhase === "connected"
        ? { phase: "connected", accountEmail: "alice@example.com", scopesRequired: ["repo"], scopesGranted: ["repo"], scopesMissing: [], activeCredentialExpiresAt: null, renewalCredentialExpiresAt: null }
        : { phase: connectionPhase, accountEmail: null, scopesRequired: ["repo"], scopesGranted: [], scopesMissing: ["repo"], activeCredentialExpiresAt: null, renewalCredentialExpiresAt: null },
    }],
    available: [{ provider: "github", displayName: "GitHub", enabled: true }],
  }));
  let browserPreferences = {
    apiVersion: ["steward", "preferences/v1"].join("."),
    onboardingDismissed: false,
    revision: 0,
    theme: null,
    workflowAcknowledged: false,
  };
  await context.route(`${origin}/app/api/v1/preferences`, async (route) => {
    if (route.request().method() === "PUT") {
      const body = route.request().postDataJSON();
      mutations.push({
        path: "/app/api/v1/preferences",
        headers: route.request().headers(),
        body,
      });
      browserPreferences = {
        ...browserPreferences,
        ...body,
        revision: browserPreferences.revision + 1,
      };
    }
    await json(route, browserPreferences);
  });
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
  return { context, page, consoleErrors, crossOriginRequests, executionLogRequests, httpErrors, mutations };
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
        const activeLink = route.activeNavigation === "Approvals"
          ? session.page.getByRole("link", { name: /^Approvals/ })
          : session.page.getByRole("link", { name: route.activeNavigation, exact: true });
        await expect(activeLink).toHaveAttribute("aria-current", "page");
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
    const provisioned = developer.page.getByRole("article").getByText("provisioned", { exact: true });
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
    await developer.page.getByRole("checkbox", { name: "I understand this revokes the shared Steward connection." }).check();
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

test("onboarding persists workflow acknowledgement and ignores unrelated runs", async ({ browser }) => {
  const developer = await guardedPage(browser, { includeSampleWorkflow: true });
  try {
    await developer.page.goto(`${origin}/get-started`);
    const workflowStep = developer.page.getByRole("listitem").filter({ hasText: "3. Add the generated workflow" });
    const runStep = developer.page.getByRole("listitem").filter({ hasText: "4. Run the test workflow" });
    await expect(workflowStep.getByText("pending", { exact: true })).toBeVisible();
    await expect(runStep.getByText("pending", { exact: true })).toBeVisible();

    await developer.page.getByRole("button", { name: "I added the sample workflow" }).click();
    await expect(workflowStep.getByText("done", { exact: true })).toBeVisible();
    const acknowledgement = developer.mutations.find((mutation) => mutation.path === "/app/api/v1/preferences");
    expect(acknowledgement, "expected the durable preference mutation").toBeTruthy();
    expect(acknowledgement.headers["x-steward-csrf"]).toBe("test-csrf");
    expect(acknowledgement.headers["content-type"]).toContain("application/json");
    expect(acknowledgement.headers.origin).toBe(origin);
    expect(acknowledgement.body).toEqual({ workflowAcknowledged: true });

    await developer.page.reload();
    await expect(developer.page.getByRole("listitem").filter({ hasText: "3. Add the generated workflow" }).getByText("done", { exact: true })).toBeVisible();
    await expect(developer.page.getByRole("listitem").filter({ hasText: "4. Run the test workflow" }).getByText("pending", { exact: true })).toBeVisible();
  } finally {
    await closeGuardedPage(developer);
  }
});

test("onboarding cannot acknowledge an absent sample", async ({ browser }) => {
  const developer = await guardedPage(browser);
  try {
    await developer.page.goto(`${origin}/get-started`);
    await expect(developer.page.getByText("The deployment has no executable onboarding sample.")).toBeVisible();
    await expect(developer.page.getByRole("button", { name: "I added the sample workflow" })).toHaveCount(0);
  } finally {
    await closeGuardedPage(developer);
  }
});

test("onboarding follows paginated envelope and run evidence", async ({ browser }) => {
  const developer = await guardedPage(browser, {
    includeSampleWorkflow: true,
    onboardingPagination: true,
  });
  try {
    await developer.page.goto(`${origin}/get-started`);
    await expect(developer.page.getByRole("listitem").filter({ hasText: "2. Provision an envelope" }).getByText("done", { exact: true })).toBeVisible();
    await expect(developer.page.getByRole("listitem").filter({ hasText: "4. Run the test workflow" }).getByText("done", { exact: true })).toBeVisible();
    await expect(developer.page.getByRole("listitem").filter({ hasText: "5. Inspect the governed run" }).getByText("done", { exact: true })).toBeVisible();
  } finally {
    await closeGuardedPage(developer);
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

test("GitHub re-run polls with one idempotency key until the new Task is correlated", async ({ browser }) => {
  const developer = await guardedPage(browser, {
    rerunResponses: [
      { status: 202, body: { apiVersion: "steward.browser-runs/v1", state: "pending", retryAfterMs: 1 } },
      { status: 201, body: { apiVersion: "steward.browser-runs/v1", taskUid: rerunTaskUid } },
    ],
  });
  try {
    await developer.page.goto(`${origin}/runs/${taskUid}`);
    await developer.page.getByRole("button", { name: "Re-run" }).click();
    await expect(developer.page.getByRole("button", { name: "Starting…" })).toBeDisabled();
    await expect(developer.page).toHaveURL(`${origin}/runs/${rerunTaskUid}`);

    const reruns = developer.mutations.filter((mutation) => mutation.path.endsWith("/rerun"));
    expect(reruns).toHaveLength(2);
    for (const mutation of reruns) expectMutationProof(mutation);
    expect(reruns[0].body.idempotencyKey).toBeTruthy();
    expect(reruns[1].body.idempotencyKey).toBe(reruns[0].body.idempotencyKey);
  } finally {
    await closeGuardedPage(developer);
  }
});

test("failed run phases expose stdout and stderr as escaped sensitive output", async ({ browser }) => {
  const developer = await guardedPage(browser, {
    executionLogs: {
      stdout: { body: "checked repository\n<script id=executed>bad()</script>\n", status: 200 },
      stderr: { body: "tool call failed: example\n", status: 200 },
    },
    runPhase: "failed",
  });
  try {
    await developer.context.addCookies([{ name: "execution-log-session", value: "present", url: origin }]);
    await developer.page.goto(`${origin}/runs/${taskUid}`);
    const failedPhase = developer.page.locator("li").filter({ has: developer.page.getByText("failed", { exact: true }) });
    const stdoutLink = failedPhase.getByRole("link", { name: "View stdout" });
    const stderrLink = failedPhase.getByRole("link", { name: "View stderr" });
    await expect(stdoutLink).toHaveAttribute("href", `/runs/${taskUid}/logs/stdout`);
    await expect(stderrLink).toHaveAttribute("href", `/runs/${taskUid}/logs/stderr`);

    await stdoutLink.click();
    await expect(developer.page).toHaveURL(`${origin}/runs/${taskUid}/logs/stdout`);
    const viewer = developer.page.getByRole("region", { name: "Execution log" });
    await expect(viewer.getByRole("heading", { name: "stdout log" })).toBeVisible();
    await expect(viewer.getByText("Sensitive output warning", { exact: true })).toBeVisible();
    await expect(viewer.locator("pre")).toHaveText("checked repository\n<script id=executed>bad()</script>\n");
    await expect(developer.page.locator("#executed")).toHaveCount(0);

    await developer.page.getByRole("link", { name: "Back to run" }).click();
    await expect(developer.page).toHaveURL(`${origin}/runs/${taskUid}`);
    await developer.page.getByRole("link", { name: "View stderr" }).click();
    await expect(developer.page).toHaveURL(`${origin}/runs/${taskUid}/logs/stderr`);
    await expect(viewer.getByRole("heading", { name: "stderr log" })).toBeVisible();
    await expect(viewer.locator("pre")).toHaveText("tool call failed: example\n");

    expect(developer.executionLogRequests.map((request) => new URL(request.url()).pathname)).toEqual([
      `/app/api/v1/runs/${taskUid}/logs/stdout`,
      `/app/api/v1/runs/${taskUid}/logs/stderr`,
    ]);
    for (const request of developer.executionLogRequests) {
      expect(new URL(request.url()).origin).toBe(origin);
      expect(request.headers().accept).toBe("application/json");
      expect(request.headers().cookie).toContain("execution-log-session=present");
    }
  } finally {
    await closeGuardedPage(developer);
  }
});

test("administrator run logs use the administrator boundary and report unavailable logs", async ({ browser }) => {
  const administrator = await guardedPage(browser, {
    executionLogs: {
      stdout: { body: "", status: 404 },
      stderr: { body: "", status: 404 },
    },
    expectedHttpStatuses: [404],
    runPhase: "succeeded",
    session: administratorSession,
  });
  try {
    await administrator.page.goto(`${origin}/admin/runs/${taskUid}`);
    const succeededPhase = administrator.page.locator("li").filter({ has: administrator.page.getByText("succeeded", { exact: true }) });
    await succeededPhase.getByRole("link", { name: "View stderr" }).click();
    await expect(administrator.page).toHaveURL(`${origin}/admin/runs/${taskUid}/logs/stderr`);
    await expect(administrator.page.getByRole("status")).toHaveText("stderr log is unavailable for this run.");
    expect(administrator.executionLogRequests).toHaveLength(1);
    expect(new URL(administrator.executionLogRequests[0].url()).pathname).toBe(
      `/admin/api/v1/all-runs/${taskUid}/logs/stderr`,
    );
  } finally {
    await closeGuardedPage(administrator);
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
    await expect(administrator.page.getByLabel("Agent")).toHaveValue("codex@0.140.0");
    await administrator.page.getByLabel("Prompt").fill("Analyze the repository state.");
    await administrator.page.getByRole("button", { name: "Publish workflow" }).click();
    await expect(administrator.page).toHaveURL(`${origin}/admin/workflows/repository-analysis/versions/1`);
    const initial = administrator.mutations.find((mutation) => mutation.path === "/admin/api/v1/workflows");
    expectMutationProof(initial);
    expect(initial.body).toEqual({
      agent: "codex@0.140.0",
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
      agent: "codex@0.140.0",
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
    await expect(administrator.page.getByRole("group", { name: "Models" }).getByRole("listitem")).toHaveText(["provider-a/model-a"]);
    const model = administrator.page.getByRole("combobox", { name: "Model" });
    await expect(model).toHaveValue(JSON.stringify(["provider-a", "model-a"]));
    await expect(model.locator("option")).toHaveText([
      "provider-a/model-a",
      "provider-b/model-b",
    ]);
    await expect(model.locator("option").nth(0)).toBeEnabled();
    await expect(model.locator("option").nth(1)).toBeEnabled();
    await expect(model.locator("option", { hasText: "openai/gpt-5.4" })).toHaveCount(0);
    await model.selectOption(JSON.stringify(["provider-b", "model-b"]));
    await administrator.page.getByRole("button", { name: "Add model" }).click();
    await expect(administrator.page.getByRole("group", { name: "Models" }).getByRole("listitem")).toHaveText([
      "provider-a/model-a",
      "provider-b/model-b",
    ]);

    const toolProvider = administrator.page.getByRole("combobox", { name: "Tool provider" });
    await expect(toolProvider).toHaveValue("github");
    await expect(toolProvider.locator("option")).toHaveText(["GitHub"]);

    const tool = administrator.page.getByRole("combobox", { exact: true, name: "Tool" });
    await expect(tool).toHaveValue(JSON.stringify(["github", "repository", "get_file_contents"]));
    await expect(tool.locator("option")).toHaveText(["repository:get_file_contents"]);
    await expect(tool.locator("option").nth(0)).toBeEnabled();
    await expect(administrator.page.getByRole("button", { name: "Add tool" })).toBeDisabled();
    const advanced = administrator.page.getByText("Advanced", { exact: true }).locator("..");
    await expect(advanced).not.toHaveAttribute("open", "");
    await administrator.page.getByRole("button", { name: "Save new version" }).click();
    await expect(administrator.page.getByText("Template revision accepted by the Rust authority.")).toBeVisible();
    await expect(administrator.page.getByText("Current revision 5")).toBeVisible();
    const templateMutation = administrator.mutations.find((mutation) => mutation.path === "/admin/api/v1/envelope-templates/analyst");
    expectMutationProof(templateMutation);
    expect(templateMutation.body.displayName).toBe("Analyst");
    expect(templateMutation.body.memberRoles).toEqual(["analyst"]);
    expect(templateMutation.body.envelope.revision).toBe(5);
    expect(templateMutation.body.envelope.spec.budget).toEqual({
      currency: "USD",
      monthlyLimit: "30.00",
      singleRunLimit: "3.00",
    });
    expect(templateMutation.body.envelope.spec.llms).toEqual(capabilityCatalog.models);
    await administrator.page.getByRole("textbox", { name: "New template ID" }).fill("reviewer");
    await administrator.page.getByRole("button", { name: "Save as new" }).click();
    await expect.poll(() => administrator.mutations.some((mutation) => mutation.path === "/admin/api/v1/envelope-templates/reviewer")).toBe(true);
    const copiedTemplate = administrator.mutations.find((mutation) => mutation.path === "/admin/api/v1/envelope-templates/reviewer");
    expectMutationProof(copiedTemplate);
    expect(copiedTemplate.body.envelope.revision).toBe(1);

    await administrator.page.goto(`${origin}/admin/envelopes/templates/new`);
    await expect(administrator.page.getByRole("group", { name: "Models" }).getByRole("listitem")).toHaveText(["provider-a/model-a"]);
    await expect(administrator.page.getByRole("combobox", { name: "Model" }).locator("option")).toHaveText([
      "provider-a/model-a",
      "provider-b/model-b",
    ]);

    await administrator.page.goto(`${origin}/admin/approvals`);
    await expect(administrator.page.getByRole("heading", { name: "Requests" })).toBeVisible();
    const envelopeRequestCard = administrator.page.getByRole("listitem").filter({
      has: administrator.page.getByText(pendingEnvelopeRequest.requestId, { exact: true }),
    });
    await expect(envelopeRequestCard).toBeVisible();
    await expect(envelopeRequestCard.getByText("alice@example.com")).toBeVisible();
    await expect(envelopeRequestCard.getByText("envelope_request", { exact: true })).toBeVisible();
    await expect(envelopeRequestCard.getByRole("heading", { name: "Requested changes" })).toBeVisible();
    await expect(envelopeRequestCard.getByRole("button", { name: "Reject request" })).toBeVisible();
    await envelopeRequestCard.getByLabel("Rationale", { exact: true }).fill("Approved for the requested bounded envelope.");
    await envelopeRequestCard.getByRole("button", { name: "Approve", exact: true }).click();
    await expect(envelopeRequestCard.getByText("Approval applied through the governed Rust admission path.")).toBeVisible();
    const envelopeApprovalMutation = administrator.mutations.find((mutation) => mutation.path === `/admin/api/v1/envelope-requests/${pendingEnvelopeRequest.requestId}/approve`);
    expectMutationProof(envelopeApprovalMutation);
    expect(envelopeApprovalMutation.body.rationale).toBe("Approved for the requested bounded envelope.");
    const runtimeApprovalCard = administrator.page.getByRole("listitem").filter({
      has: administrator.page.getByText(approvalId, { exact: true }),
    });
    await runtimeApprovalCard.getByRole("button", { name: "File decision reference" }).click();
    await expect(runtimeApprovalCard.getByText("Decision reference filed through the server-owned channel.")).toBeVisible();
    expectMutationProof(administrator.mutations.find((mutation) => mutation.path === `/admin/api/v1/approvals/${approvalId}/file`));
    await runtimeApprovalCard.getByLabel("Rationale", { exact: true }).fill("Approved for one bounded investigation.");
    await runtimeApprovalCard.getByLabel("Expires at (RFC 3339)").fill("2026-08-25T17:00:00Z");
    await runtimeApprovalCard.getByRole("button", { name: "Approve", exact: true }).click();
    await expect(runtimeApprovalCard.getByText("Approval applied through the governed Rust admission path.")).toBeVisible();
    const approvalMutation = administrator.mutations.find((mutation) => mutation.path === `/admin/api/v1/approvals/${approvalId}/approve`);
    expectMutationProof(approvalMutation);
    expect(approvalMutation.body.evidenceUrl).toBe("https://example.com/decisions/PROJ-123");

    await administrator.page.goto(`${origin}/admin/settings`);
    await expect(administrator.page.getByRole("heading", { name: "Administrator session" })).toBeVisible();
  } finally {
    await closeGuardedPage(administrator);
  }
});

test("administrator template model controls fail closed when the capability catalog has no models", async ({ browser }) => {
  const administrator = await guardedPage(browser, {
    capabilityCatalogModels: [],
    session: administratorSession,
  });
  try {
    await administrator.page.goto(`${origin}/admin/envelopes/templates/analyst`);
    const model = administrator.page.getByRole("combobox", { name: "Model" });
    await expect(model).toBeDisabled();
    await expect(model.locator("option")).toHaveText(["No models available"]);
    await expect(administrator.page.getByRole("button", { name: "Add model" })).toBeDisabled();
    await expect(administrator.page.getByText("No models are listed in the deployment capability catalog.", { exact: true })).toBeVisible();
    await expect(model.locator("option", { hasText: "openai/gpt-5.4" })).toHaveCount(0);
  } finally {
    await closeGuardedPage(administrator);
  }
});

test("administrator can revise and copy a ten-grant template listed in the capability catalog", async ({ browser }) => {
  const administrator = await guardedPage(browser, {
    adminTemplateTools: githubReadTools,
    capabilityCatalogTools: githubReadTools,
    session: administratorSession,
  });
  try {
    await administrator.page.goto(`${origin}/admin/envelopes/templates/developer`);
    const tools = administrator.page.getByRole("group", { name: "Tools" });
    await expect(tools.getByRole("listitem")).toHaveCount(10);
    await expect(tools.getByRole("combobox", { name: "Tool", exact: true }).locator("option")).toHaveCount(10);

    await administrator.page.getByRole("button", { name: "Save new version" }).click();
    await expect.poll(() => administrator.mutations.find((mutation) => mutation.path === "/admin/api/v1/envelope-templates/developer")).toBeTruthy();
    const revised = administrator.mutations.find((mutation) => mutation.path === "/admin/api/v1/envelope-templates/developer");
    expectMutationProof(revised);
    expect(revised.body.envelope.revision).toBe(adminEnvelope.revision + 1);
    expect(revised.body.envelope.spec.tools).toEqual(githubReadTools.map(({ accessClass: _accessClass, ...tool }) => tool));

    await administrator.page.getByRole("textbox", { name: "New template ID" }).fill("engineer");
    await administrator.page.getByRole("button", { name: "Save as new" }).click();
    await expect.poll(() => administrator.mutations.find((mutation) => mutation.path === "/admin/api/v1/envelope-templates/engineer")).toBeTruthy();
    const copied = administrator.mutations.find((mutation) => mutation.path === "/admin/api/v1/envelope-templates/engineer");
    expectMutationProof(copied);
    expect(copied.body.envelope.revision).toBe(1);
    expect(copied.body.envelope.spec.tools).toEqual(githubReadTools.map(({ accessClass: _accessClass, ...tool }) => tool));
  } finally {
    await closeGuardedPage(administrator);
  }
});

test("administrator can replace a template tool absent from the capability catalog", async ({ browser }) => {
  const replacement = githubReadTools[0];
  const administrator = await guardedPage(browser, {
    capabilityCatalogTools: [replacement],
    session: administratorSession,
  });
  try {
    await administrator.page.goto(`${origin}/admin/envelopes/templates/analyst`);
    const tools = administrator.page.getByRole("group", { name: "Tools" });
    await expect(tools.getByText("Not listed in the deployment capability catalog")).toBeVisible();
    await expect(tools.getByRole("combobox", { name: "Tool", exact: true }).locator("option")).toHaveText(["actions_get:read"]);
    await administrator.page.getByRole("button", { name: "Save new version" }).click();
    await expect(administrator.page.getByRole("alert").filter({ hasText: "no authority was changed" })).toBeVisible();
    expect(administrator.mutations.some((mutation) => mutation.path === "/admin/api/v1/envelope-templates/analyst")).toBe(false);

    await tools.getByRole("button", { name: "Remove tool github:repository:get_file_contents" }).click();
    await tools.getByRole("button", { name: "Add tool" }).click();
    await administrator.page.getByRole("button", { name: "Save new version" }).click();
    await expect.poll(() => administrator.mutations.find((mutation) => mutation.path === "/admin/api/v1/envelope-templates/analyst")).toBeTruthy();
    expect(administrator.mutations.find((mutation) => mutation.path === "/admin/api/v1/envelope-templates/analyst").body.envelope.spec.tools).toEqual([
      { provider: replacement.provider, resource: replacement.resource, action: replacement.action },
    ]);
  } finally {
    await closeGuardedPage(administrator);
  }
});

test("administrator template tool controls offer no fallback when the capability catalog has no tools", async ({ browser }) => {
  const administrator = await guardedPage(browser, {
    adminTemplateTools: [],
    capabilityCatalogTools: [],
    session: administratorSession,
  });
  try {
    await administrator.page.goto(`${origin}/admin/envelopes/templates/analyst`);
    const tools = administrator.page.getByRole("group", { name: "Tools" });
    await expect(tools.getByRole("combobox", { name: "Tool provider" })).toBeDisabled();
    await expect(tools.getByRole("combobox", { name: "Tool", exact: true })).toBeDisabled();
    await expect(tools.getByRole("button", { name: "Add tool" })).toBeDisabled();
    await expect(tools.getByText("No tools are listed in the deployment capability catalog.")).toBeVisible();
    await administrator.page.getByRole("button", { name: "Save new version" }).click();
    await expect.poll(() => administrator.mutations.find((mutation) => mutation.path === "/admin/api/v1/envelope-templates/analyst")).toBeTruthy();
    expect(administrator.mutations.find((mutation) => mutation.path === "/admin/api/v1/envelope-templates/analyst").body.envelope.spec.tools).toEqual([]);
  } finally {
    await closeGuardedPage(administrator);
  }
});

test("administrator template authoring rejects models absent from the capability catalog", async ({ browser }) => {
  const administrator = await guardedPage(browser, {
    capabilityCatalogModels: [{ provider: "provider-b", model: "model-b" }],
    session: administratorSession,
  });
  try {
    await administrator.page.goto(`${origin}/admin/envelopes/templates/analyst`);
    await administrator.page.getByRole("button", { name: "Save new version" }).click();
    await expect(administrator.page.getByText("The template ID or envelope fields are invalid, so no authority was changed.", { exact: true })).toBeVisible();
    expect(administrator.mutations.some((mutation) => mutation.path === "/admin/api/v1/envelope-templates/analyst")).toBe(false);
  } finally {
    await closeGuardedPage(administrator);
  }
});

test("administrator can replace a template model absent from the capability catalog", async ({ browser }) => {
  const administrator = await guardedPage(browser, {
    adminTemplateModels: [{ provider: "openai", model: "gpt-5.4" }],
    capabilityCatalogModels: [{ provider: "anthropic", model: "claude-sonnet-4" }],
    session: administratorSession,
  });
  try {
    await administrator.page.goto(`${origin}/admin/envelopes/templates/analyst`);
    const models = administrator.page.getByRole("group", { name: "Models" });
    await expect(models.getByRole("listitem")).toContainText("openai/gpt-5.4");
    await expect(models.getByText("Not listed in the deployment capability catalog")).toBeVisible();
    await administrator.page.getByRole("button", { name: "Save new version" }).click();
    await expect(administrator.page.getByText("The template ID or envelope fields are invalid, so no authority was changed.", { exact: true })).toBeVisible();
    expect(administrator.mutations.some((mutation) => mutation.path === "/admin/api/v1/envelope-templates/analyst")).toBe(false);

    await models.getByRole("button", { name: "Remove model gpt-5.4 from provider openai" }).click();
    await expect(models.getByRole("listitem")).toHaveCount(0);
    await models.getByRole("combobox", { name: "Model" }).selectOption(JSON.stringify(["anthropic", "claude-sonnet-4"]));
    await models.getByRole("button", { name: "Add model" }).click();
    await expect(models.getByRole("listitem")).toHaveCount(1);
    await administrator.page.getByRole("button", { name: "Save new version" }).click();
    await expect.poll(() => administrator.mutations.find((mutation) => mutation.path === "/admin/api/v1/envelope-templates/analyst")).toBeTruthy();
    const mutation = administrator.mutations.find((entry) => entry.path === "/admin/api/v1/envelope-templates/analyst");
    expect(mutation.body.envelope.spec.llms).toEqual([{ provider: "anthropic", model: "claude-sonnet-4" }]);
    expect(mutation.body.envelope.revision).toBe(adminEnvelope.revision + 1);
  } finally {
    await closeGuardedPage(administrator);
  }
});

test("administrator template model identity does not collide across provider and model slashes", async ({ browser }) => {
  const first = { provider: "provider-a", model: "part/model-a" };
  const second = { provider: "provider-a/part", model: "model-a" };
  const administrator = await guardedPage(browser, {
    adminTemplateModels: [first],
    capabilityCatalogModels: [first, second],
    session: administratorSession,
  });
  try {
    await administrator.page.goto(`${origin}/admin/envelopes/templates/analyst`);
    const models = administrator.page.getByRole("group", { name: "Models" });
    await expect(models.getByRole("combobox", { name: "Model" }).locator("option")).toHaveText([
      "Provider: provider-a · Model: part/model-a",
      "Provider: provider-a/part · Model: model-a",
    ]);
    await models.getByRole("combobox", { name: "Model" }).selectOption(JSON.stringify([second.provider, second.model]));
    await models.getByRole("button", { name: "Add model" }).click();
    await expect(models.getByRole("listitem")).toHaveCount(2);
    await models.getByRole("listitem").first().getByRole("button", { name: "Remove model part/model-a from provider provider-a" }).click();
    await expect(models.getByRole("listitem")).toHaveCount(1);
    await administrator.page.getByRole("button", { name: "Save new version" }).click();
    await expect.poll(() => administrator.mutations.find((mutation) => mutation.path === "/admin/api/v1/envelope-templates/analyst")).toBeTruthy();
    expect(administrator.mutations.find((mutation) => mutation.path === "/admin/api/v1/envelope-templates/analyst").body.envelope.spec.llms).toEqual([second]);
  } finally {
    await closeGuardedPage(administrator);
  }

  const stale = await guardedPage(browser, {
    adminTemplateModels: [first, second],
    capabilityCatalogModels: [second],
    session: administratorSession,
  });
  try {
    await stale.page.goto(`${origin}/admin/envelopes/templates/analyst`);
    const models = stale.page.getByRole("group", { name: "Models" });
    await expect(models.getByText("Not listed in the deployment capability catalog")).toHaveCount(1);
    await models.getByRole("listitem").first().getByRole("button", { name: "Remove model part/model-a from provider provider-a" }).click();
    await expect(models.getByRole("listitem")).toHaveCount(1);
    await stale.page.getByRole("button", { name: "Save new version" }).click();
    await expect.poll(() => stale.mutations.find((mutation) => mutation.path === "/admin/api/v1/envelope-templates/analyst")).toBeTruthy();
    expect(stale.mutations.find((mutation) => mutation.path === "/admin/api/v1/envelope-templates/analyst").body.envelope.spec.llms).toEqual([second]);
  } finally {
    await closeGuardedPage(stale);
  }
});

test("administrator template authoring has no fallback when the capability catalog is unavailable", async ({ browser }) => {
  const administrator = await guardedPage(browser, {
    expectedHttpStatuses: [404],
    capabilityCatalogStatus: 404,
    session: administratorSession,
  });
  try {
    await administrator.page.goto(`${origin}/admin/envelopes/templates/analyst`);
    await expect(administrator.page.getByRole("heading", { name: "Not found" })).toBeVisible();
    await expect(administrator.page.getByRole("combobox", { name: "Model" })).toHaveCount(0);
    await expect(administrator.page.getByText("openai/gpt-5.4", { exact: true })).toHaveCount(0);
  } finally {
    await closeGuardedPage(administrator);
  }
});

test("administrator can reject a pending envelope request with an optional reason", async ({ browser }) => {
  const administrator = await guardedPage(browser, { session: administratorSession });
  try {
    await administrator.page.goto(`${origin}/admin/approvals`);
    const envelopeRequestCard = administrator.page.getByRole("listitem").filter({
      has: administrator.page.getByText(pendingEnvelopeRequest.requestId, { exact: true }),
    });
    await envelopeRequestCard.getByLabel("Rejection reason (optional)").fill("Authority is not appropriate for this user.");
    await envelopeRequestCard.getByRole("button", { name: "Reject request" }).click();
    await expect(envelopeRequestCard.getByText("The envelope request was rejected through the governed Rust admission path.")).toBeVisible();
    const rejection = administrator.mutations.find((mutation) => mutation.path === `/admin/api/v1/envelope-requests/${pendingEnvelopeRequest.requestId}/reject`);
    expectMutationProof(rejection);
    expect(rejection.body).toEqual({ reason: "Authority is not appropriate for this user." });
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
    await expect(administrator.page.getByRole("group", { name: "Models" }).getByRole("listitem")).toHaveText(["provider-a/model-a"]);
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
    mutationFailures: { "/app/api/v1/connections/github/start": 403 },
  });
  try {
    await developer.page.goto(`${origin}/connections`);
    await developer.page.getByRole("button", { name: "Connect GitHub" }).click();
    await expect(developer.page.getByText("The Rust authorization boundary rejected the connection action.")).toBeVisible();
  } finally {
    await closeGuardedPage(developer);
  }
});
