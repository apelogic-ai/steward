import { spawnSync } from "node:child_process";
import { createHmac } from "node:crypto";
import { readFileSync, realpathSync, statSync } from "node:fs";
import { basename, dirname, isAbsolute, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const MAX_EXCHANGE_BYTES = 64 * 1024;
const MAX_JSON_BYTES = 1024 * 1024;
const MAX_OUTPUT_BYTES = 32 * 1024 * 1024;
const REQUEST_TIMEOUT_MS = 30_000;
const JOURNEY_CHECKOUT_ROOT = resolve(fileURLToPath(new URL("..", import.meta.url)));

function required(name, environment = process.env) {
  const value = environment[name]?.trim();
  if (!value) throw new Error(`${name} is required for @callers journeys.`);
  return value;
}

function isLoopback(hostname) {
  return ["127.0.0.1", "localhost", "[::1]"].includes(hostname);
}

export function exactHttpUrl(name, value, { path = null } = {}) {
  let url;
  try {
    url = new URL(value);
  } catch {
    throw new Error(`${name} must be an absolute URL.`);
  }
  if ((url.protocol !== "https:" && !(url.protocol === "http:" && isLoopback(url.hostname)))
      || url.username || url.password || url.hash || url.search) {
    throw new Error(`${name} must use verified HTTPS or loopback HTTP without credentials, query or fragment.`);
  }
  if (path !== null && url.pathname !== path) {
    throw new Error(`${name} must use the exact path ${path}.`);
  }
  return url;
}

export function adminConfiguration(environment = process.env) {
  const tokenFile = required("STEWARD_JOURNEY_ADMIN_TOKEN_FILE", environment);
  if (!isAbsolute(tokenFile)) {
    throw new Error("STEWARD_JOURNEY_ADMIN_TOKEN_FILE must use an absolute path.");
  }
  return {
    apiOrigin: exactHttpUrl(
      "STEWARD_JOURNEY_BASE_URL",
      required("STEWARD_JOURNEY_BASE_URL", environment),
    ).origin,
    tokenFile,
  };
}

function adminToken(config, dependencies) {
  let repositoryRoot;
  let tokenFile;
  try {
    const realpathFile = dependencies.realpathFile ?? realpathSync;
    const checkoutRoot = realpathFile(dependencies.checkoutRoot ?? JOURNEY_CHECKOUT_ROOT);
    const checkoutParent = dirname(checkoutRoot);
    repositoryRoot = basename(checkoutParent) === ".worktrees"
      ? dirname(checkoutParent)
      : checkoutRoot;
    tokenFile = realpathFile(config.tokenFile);
  } catch {
    throw new Error("The administrator token file could not be read.");
  }
  const repositoryRelative = relative(repositoryRoot, tokenFile);
  if (repositoryRelative === ""
      || (!repositoryRelative.startsWith(`..${sep}`)
          && repositoryRelative !== ".."
          && !isAbsolute(repositoryRelative))) {
    throw new Error("The administrator token file must be outside the repository.");
  }
  let metadata;
  try {
    metadata = (dependencies.statFile ?? statSync)(tokenFile);
  } catch {
    throw new Error("The administrator token file could not be read.");
  }
  if (!metadata.isFile() || (metadata.mode & 0o777) !== 0o600) {
    throw new Error("The administrator token file must be a regular file with mode 0600.");
  }
  let contents;
  try {
    contents = (dependencies.readFile ?? readFileSync)(tokenFile);
  } catch {
    throw new Error("The administrator token file could not be read.");
  }
  const bytes = Buffer.isBuffer(contents) ? contents : Buffer.from(contents);
  if (!bytes.length || bytes.length > MAX_EXCHANGE_BYTES) {
    throw new Error("The administrator token file is empty or too large.");
  }
  const token = bytes.toString("utf8").trim();
  if (!token || /\s/.test(token)) {
    throw new Error("The administrator token file does not contain one valid credential.");
  }
  return token;
}

export async function adminRequest(config, path, dependencies = {}) {
  const url = new URL(path, `${config.apiOrigin}/`);
  if (url.origin !== config.apiOrigin
      || (url.pathname !== "/admin/api/v1/runs" && !url.pathname.startsWith("/admin/api/v1/runs/"))) {
    throw new Error("The administrator credential may be sent only to an administrator Runs path.");
  }
  return request(dependencies.fetchImpl ?? fetch, url, {
    headers: { authorization: `Bearer ${adminToken(config, dependencies)}` },
    method: "GET",
  });
}

export function callerConfiguration(name, environment = process.env) {
  const key = name.toUpperCase();
  if (!["JENKINS", "TEMPORAL"].includes(key)) throw new Error(`Unsupported caller ${name}.`);
  const workflow = required(`STEWARD_JOURNEY_${key}_WORKFLOW`, environment);
  if (!/^[^@\s]+@[1-9][0-9]*$/.test(workflow)) {
    throw new Error(`STEWARD_JOURNEY_${key}_WORKFLOW must be a versioned name such as release-summary@1.`);
  }
  return {
    apiOrigin: exactHttpUrl("STEWARD_JOURNEY_BASE_URL", required("STEWARD_JOURNEY_BASE_URL", environment)).origin,
    exchangeUrl: exactHttpUrl(
      `STEWARD_JOURNEY_${key}_EXCHANGE_URL`,
      required(`STEWARD_JOURNEY_${key}_EXCHANGE_URL`, environment),
      { path: key === "JENKINS" ? "/v1/oidc/exchange" : "/v1/workload/exchange" },
    ).toString(),
    sourceHelper: required(`STEWARD_JOURNEY_${key}_SOURCE_HELPER`, environment),
    workflow,
  };
}

export function unboundConfiguration(environment = process.env) {
  const exchangeUrl = exactHttpUrl(
    "STEWARD_JOURNEY_UNBOUND_EXCHANGE_URL",
    required("STEWARD_JOURNEY_UNBOUND_EXCHANGE_URL", environment),
  );
  if (!["/v1/oidc/exchange", "/v1/workload/exchange"].includes(exchangeUrl.pathname)) {
    throw new Error("STEWARD_JOURNEY_UNBOUND_EXCHANGE_URL must use a supported exchange path.");
  }
  return {
    exchangeUrl: exchangeUrl.toString(),
    sourceHelper: required("STEWARD_JOURNEY_UNBOUND_SOURCE_HELPER", environment),
  };
}

export function requireLiveJiraAuthorization(environment = process.env) {
  const runPrefix = required("STEWARD_JOURNEY_RUN_PREFIX", environment);
  const authorization = required("STEWARD_JOURNEY_ALLOW_LIVE_JIRA_WEBHOOK", environment);
  if (authorization !== runPrefix) {
    throw new Error("A live Jira webhook requires an owner instruction for this run prefix.");
  }
}

export function sourceAssertionFromHelper(helper, spawn = spawnSync) {
  if (!isAbsolute(helper)) throw new Error("The caller source helper must use an absolute path.");
  const result = spawn(helper, [], {
    encoding: "utf8",
    maxBuffer: MAX_EXCHANGE_BYTES,
    stdio: ["ignore", "pipe", "pipe"],
    timeout: 10_000,
  });
  if (result.error || result.signal || result.status !== 0) {
    throw new Error("The caller source helper did not produce a credential.");
  }
  const assertion = result.stdout.trim();
  if (!assertion || /\s/.test(assertion) || Buffer.byteLength(assertion) > MAX_EXCHANGE_BYTES) {
    throw new Error("The caller source helper returned an invalid credential.");
  }
  return assertion;
}

export function runLiveJiraHelper(helper, spawn = spawnSync) {
  if (!isAbsolute(helper)) throw new Error("The live Jira helper must use an absolute path.");
  const result = spawn(helper, [], {
    stdio: "ignore",
    timeout: 30_000,
  });
  if (result.error || result.signal || result.status !== 0) {
    throw new Error("The live Jira helper did not complete successfully.");
  }
}

async function bodyLimited(response, limit, label) {
  const declared = Number(response.headers.get("content-length"));
  if (Number.isFinite(declared) && declared > limit) throw new Error(`${label} was too large.`);
  const body = Buffer.from(await response.arrayBuffer());
  if (body.length > limit) throw new Error(`${label} was too large.`);
  return body;
}

async function jsonLimited(response, limit, label) {
  const body = await bodyLimited(response, limit, label);
  try {
    return JSON.parse(body.toString("utf8"));
  } catch {
    throw new Error(`${label} was not valid JSON.`);
  }
}

async function request(fetchImpl, url, options = {}) {
  const response = await fetchImpl(url, {
    ...options,
    redirect: "manual",
    signal: options.signal ?? AbortSignal.timeout(REQUEST_TIMEOUT_MS),
  });
  if (response.status >= 300 && response.status < 400) {
    throw new Error("A caller endpoint attempted to redirect a credential-bearing request.");
  }
  return response;
}

export class ExchangeRefusedError extends Error {
  constructor(status) {
    super(`The token exchange refused the caller with status ${status}.`);
    this.status = status;
  }
}

export async function exchangeTaskToken(config, dependencies = {}) {
  const fetchImpl = dependencies.fetchImpl ?? fetch;
  const source = (dependencies.sourceAssertion ?? sourceAssertionFromHelper)(config.sourceHelper);
  const response = await request(fetchImpl, config.exchangeUrl, {
    body: "",
    headers: {
      accept: "application/json",
      authorization: `Bearer ${source}`,
    },
    method: "POST",
  });
  if (response.status !== 200) {
    if (response.status >= 400 && response.status < 500) throw new ExchangeRefusedError(response.status);
    throw new Error(`The token exchange was unavailable with status ${response.status}.`);
  }
  const cacheControl = response.headers.get("cache-control") ?? "";
  if (!cacheControl.split(",").some((part) => part.trim().toLowerCase() === "no-store")) {
    throw new Error("The token exchange response did not forbid storage.");
  }
  const body = await jsonLimited(response, MAX_EXCHANGE_BYTES, "The token exchange response");
  if (body.token_type !== "Bearer"
      || typeof body.access_token !== "string"
      || !body.access_token
      || body.access_token.trim() !== body.access_token
      || !Number.isInteger(body.expires_in)
      || body.expires_in < 1
      || body.expires_in > 300) {
    throw new Error("The token exchange response did not contain a valid short-lived Task token.");
  }
  return body.access_token;
}

async function taskRequest(config, path, options, dependencies) {
  const token = await exchangeTaskToken(config, dependencies);
  const response = await request(dependencies.fetchImpl ?? fetch, new URL(path, `${config.apiOrigin}/`), {
    ...options,
    headers: {
      ...options.headers,
      authorization: `Bearer ${token}`,
    },
  });
  return response;
}

export async function startCallerTask(config, idempotencyKey, dependencies = {}) {
  if (!idempotencyKey || /\s/.test(idempotencyKey)) throw new Error("The caller identity must be stable and contain no whitespace.");
  const created = await taskRequest(config, "/v1/tasks", {
    body: JSON.stringify({ workflow: config.workflow }),
    headers: {
      "content-type": "application/json",
      "idempotency-key": idempotencyKey,
    },
    method: "POST",
  }, dependencies);
  if (![200, 202].includes(created.status)) throw new Error(`The Run API refused Task creation with status ${created.status}.`);
  const task = await jsonLimited(created, MAX_JSON_BYTES, "The Task creation response");
  if (typeof task.taskUid !== "string" || !task.taskUid) throw new Error("The Task creation response omitted taskUid.");
  if (dependencies.onCreated) await dependencies.onCreated(task.taskUid);

  const uploaded = await taskRequest(config, `/v1/tasks/${encodeURIComponent(task.taskUid)}/inputs`, {
    body: Buffer.alloc(1024),
    headers: { "content-type": "application/x-tar" },
    method: "PUT",
  }, dependencies);
  if (uploaded.status !== 204) throw new Error(`The Run API refused the input archive with status ${uploaded.status}.`);

  const executed = await taskRequest(config, `/v1/tasks/${encodeURIComponent(task.taskUid)}/execute`, {
    body: "",
    method: "POST",
  }, dependencies);
  if (executed.status !== 202) throw new Error(`The Run API refused Task execution with status ${executed.status}.`);
  return { taskUid: task.taskUid };
}

export async function waitForCallerTask(config, taskUid, timeoutMs, dependencies = {}) {
  const sleep = dependencies.sleep ?? ((milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds)));
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const response = await taskRequest(config, `/v1/tasks/${encodeURIComponent(taskUid)}`, { method: "GET" }, dependencies);
    if (response.status !== 200) throw new Error(`The Run API refused Task status with status ${response.status}.`);
    const task = await jsonLimited(response, MAX_JSON_BYTES, "The Task status response");
    if (["succeeded", "failed", "cancelled"].includes(task.phase)) return task;
    if (!["submitted", "queued", "running"].includes(task.phase)) throw new Error("The Task status response contained an unknown phase.");
    await sleep(1_000);
  }
  throw new Error("The caller Task did not finish before its journey deadline.");
}

export async function downloadCallerOutput(config, taskUid, dependencies = {}) {
  const response = await taskRequest(config, `/v1/tasks/${encodeURIComponent(taskUid)}/outputs`, { method: "GET" }, dependencies);
  if (response.status !== 200) throw new Error(`The caller could not download Task output; status ${response.status}.`);
  const contentType = response.headers.get("content-type")?.split(";", 1)[0];
  if (contentType !== "application/x-tar") throw new Error("The Task output was not a tar archive.");
  return bodyLimited(response, MAX_OUTPUT_BYTES, "The Task output archive");
}

export async function deleteCallerTask(config, taskUid, dependencies = {}) {
  const response = await taskRequest(config, `/v1/tasks/${encodeURIComponent(taskUid)}`, { method: "DELETE" }, dependencies);
  if (response.status !== 202 && response.status !== 404) {
    throw new Error(`The caller could not request Task cleanup; status ${response.status}.`);
  }
}

export function outputArchiveContains(archive, expected) {
  return Buffer.isBuffer(archive) && archive.includes(Buffer.from(expected, "utf8"));
}

export function jiraPayload(issueKey, runPrefix, deliveryId) {
  const [project] = issueKey.split("-", 1);
  return Buffer.from(JSON.stringify({
    webhookEvent: "jira:issue_updated",
    issue: {
      key: issueKey,
      fields: {
        project: { key: project },
        issuetype: { id: "10000", name: "Example" },
      },
    },
    user: { accountId: "alice" },
    journeyRun: runPrefix,
    journeyDelivery: deliveryId,
  }));
}

export function jiraSignature(secret, body) {
  return `sha256=${createHmac("sha256", secret).update(body).digest("hex")}`;
}

export function invalidJiraSignature(secret, body) {
  const valid = jiraSignature(secret, body);
  const replacement = valid.endsWith("0") ? "1" : "0";
  return `${valid.slice(0, -1)}${replacement}`;
}

function requiredInteger(name, environment) {
  const value = required(name, environment);
  if (!/^[1-9][0-9]*$/.test(value)) throw new Error(`${name} must be a positive integer.`);
  return Number(value);
}

export function jiraReceiverConfiguration(mode, environment = process.env, dependencies = {}) {
  const write = mode === "write";
  if (!write && mode !== "read") throw new Error(`Unsupported Jira receiver mode ${mode}.`);
  const prefix = write ? "STEWARD_JOURNEY_JIRA_WRITE" : "STEWARD_JOURNEY_JIRA";
  const secretFile = required(`${prefix}_SIGNING_SECRET_FILE`, environment);
  if (!isAbsolute(secretFile)) throw new Error(`${prefix}_SIGNING_SECRET_FILE must use an absolute path outside the repository.`);
  const secret = (dependencies.readFile ?? readFileSync)(secretFile);
  if (!secret.length || secret.length > MAX_EXCHANGE_BYTES) throw new Error("The Jira signing secret file is empty or too large.");
  const config = {
    origin: exactHttpUrl(
      `${prefix}_RECEIVER_ORIGIN`,
      required(`${prefix}_RECEIVER_ORIGIN`, environment),
    ).origin,
    secret: Buffer.from(secret),
    trigger: required(`${prefix}_TRIGGER_ID`, environment),
  };
  if (!write) {
    config.cooldownSeconds = requiredInteger("STEWARD_JOURNEY_JIRA_COOLDOWN_SECONDS", environment);
    config.maxIssueCooldowns = requiredInteger("STEWARD_JOURNEY_JIRA_MAX_ISSUE_COOLDOWNS", environment);
    if (config.cooldownSeconds < 60) {
      throw new Error("STEWARD_JOURNEY_JIRA_COOLDOWN_SECONDS must be at least 60 for @callers journeys.");
    }
    if (config.maxIssueCooldowns < 4) {
      throw new Error("STEWARD_JOURNEY_JIRA_MAX_ISSUE_COOLDOWNS must be at least 4 for @callers journeys.");
    }
  }
  return config;
}

export async function sendJiraDelivery(config, deliveryId, body, dependencies = {}) {
  const headers = {
    "content-type": "application/json",
    "x-atlassian-webhook-identifier": deliveryId,
  };
  const signature = Object.hasOwn(dependencies, "signature")
    ? dependencies.signature
    : jiraSignature(config.secret, body);
  if (signature !== null && (typeof signature !== "string" || !/^sha256=[0-9a-f]{64}$/.test(signature))) {
    throw new Error("The Jira signature probe must be a SHA-256 signature or null.");
  }
  if (signature !== null) headers["x-hub-signature"] = signature;
  return request(dependencies.fetchImpl ?? fetch, new URL(`/hooks/jira/${encodeURIComponent(config.trigger)}`, `${config.origin}/`), {
    body,
    headers,
    method: "POST",
  });
}
