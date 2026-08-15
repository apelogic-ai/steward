"use strict";

const SESSION_VERSION = "steward.browser-session/v1";
const CONNECTIONS_VERSION = "steward.connections/v1";
const FAST_TRACK_RUNTIME_ID = "lbe259-fast-track/connections-bridge";
const FAST_TRACK_RUNTIME_PHASES = new Set([
  "pending",
  "admitted",
  "provisioning",
  "running",
  "suspended",
  "terminating",
  "terminated",
  "failed",
]);
const FAST_TRACK_POLLABLE_RUNTIME_PHASES = new Set([
  "pending",
  "admitted",
  "provisioning",
  "running",
]);
const FAST_TRACK_STATUS_POLL_INTERVAL_MS = 1000;
const FAST_TRACK_STATUS_POLL_DEADLINE_MS = 90000;
const FAST_TRACK_RETRYABLE_BFF_STAGES = new Set(["bridge_transport", "bridge_http_status"]);
const FAST_TRACK_BFF_STAGE_LABELS = new Map([
  ["lifetime_expired", "preview window expired"],
  ["session_mismatch", "browser session mismatch"],
  ["target_url", "bridge target unavailable"],
  ["bridge_transport", "waiting for bridge"],
  ["bridge_http_status", "bridge is not ready"],
  ["response_schema", "bridge response unavailable"],
  ["response_semantics", "bridge response invalid"],
]);

const fastTrackRuntimeBootstrap = document.body.dataset.fastTrackRuntime === "true";
const providerCard = document.querySelector(".provider-card");
const principalEmail = document.querySelector("#principal-email");
const canonicalUser = document.querySelector("#canonical-user");
const githubStatus = document.querySelector("#github-status");
const githubSummary = document.querySelector("#github-summary");
const githubAccount = document.querySelector("#github-account");
const githubExpiry = document.querySelector("#github-expiry");
const githubScopes = document.querySelector("#github-scopes");
const connectGithub = document.querySelector("#connect-github");
const disconnectGithub = document.querySelector("#disconnect-github");
const retryGithub = document.querySelector("#retry-github");
const disconnectDialog = document.querySelector("#disconnect-dialog");
const confirmDisconnect = document.querySelector("#confirm-disconnect");
const callbackStatus = document.querySelector("#callback-status");
const connectionError = document.querySelector("#connection-error");
const connectionErrorMessage = document.querySelector("#connection-error-message");
const runtimeStatus = document.querySelector("#runtime-status");

let csrf = null;

function showError(message) {
  connectionErrorMessage.textContent = message;
  connectionError.hidden = false;
}

function clearError() {
  connectionError.hidden = true;
}

function setBusy(busy) {
  connectGithub.disabled = busy;
  retryGithub.disabled = busy;
  confirmDisconnect.disabled = busy;
}

function mutationHeaders() {
  if (typeof csrf !== "string" || csrf.length === 0) {
    throw new Error("browser session has no CSRF proof");
  }
  return {
    "Content-Type": "application/json",
    "X-Steward-CSRF": csrf,
  };
}

function isAllowedAuthorizationUrl(value) {
  let candidate;
  try {
    candidate = new URL(value);
  } catch (_error) {
    return false;
  }
  if (candidate.username || candidate.password) {
    return false;
  }
  if (candidate.protocol === "https:") {
    return true;
  }
  const loopback = window.location.hostname === "127.0.0.1" || window.location.hostname === "[::1]";
  return (
    loopback &&
    candidate.protocol === "http:" &&
    candidate.origin === window.location.origin &&
    candidate.pathname === "/admin/connections/github/callback" &&
    candidate.searchParams.has("continuation")
  );
}

async function fetchJson(path, options = {}) {
  const response = await fetch(path, {
    cache: "no-store",
    credentials: "same-origin",
    headers: { Accept: "application/json", ...(options.headers || {}) },
    ...options,
  });
  if (!response.ok) {
    const reportedStage = response.headers.get("x-steward-fast-track-bff-stage");
    const failure = new Error("request rejected");
    failure.requestStatus = response.status;
    failure.bffStage = FAST_TRACK_BFF_STAGE_LABELS.has(reportedStage) ? reportedStage : null;
    throw failure;
  }
  return response.json();
}

function renderScopes(status) {
  const granted = new Set(status.scopesGranted);
  const items = status.scopesRequired.map((scope) => {
    const item = document.createElement("li");
    item.textContent = scope;
    item.dataset.missing = String(!granted.has(scope));
    return item;
  });
  if (items.length === 0) {
    const item = document.createElement("li");
    item.textContent = "No scopes reported";
    items.push(item);
  }
  githubScopes.replaceChildren(...items);
}

function renderConnection(status) {
  providerCard.dataset.phase = status.phase;
  githubAccount.textContent = status.accountEmail || "Not connected";
  githubExpiry.textContent = status.expiresAt || "Not reported by provider";
  renderScopes(status);
  retryGithub.hidden = true;
  disconnectGithub.hidden = true;
  connectGithub.hidden = false;
  connectGithub.disabled = false;

  if (status.phase === "connected") {
    githubStatus.textContent = "Connected";
    githubSummary.textContent = "GitHub is ready for governed runs within the scopes shown below.";
    connectGithub.hidden = true;
    disconnectGithub.hidden = false;
  } else if (status.phase === "reauth_required") {
    githubStatus.textContent = "Reconnect required";
    githubSummary.textContent = "Reconnect to restore the required GitHub capabilities.";
    connectGithub.textContent = "Reconnect GitHub";
  } else if (status.phase === "connecting") {
    githubStatus.textContent = "Connecting";
    githubSummary.textContent = "Finish the GitHub consent window, then return here.";
    connectGithub.disabled = true;
  } else if (status.phase === "unavailable") {
    githubStatus.textContent = "Unavailable";
    githubSummary.textContent = "The provider broker cannot report connection status right now.";
    connectGithub.hidden = true;
    retryGithub.hidden = false;
  } else {
    githubStatus.textContent = "Not connected";
    githubSummary.textContent = "Connect GitHub before requesting an envelope that uses GitHub tools.";
    connectGithub.textContent = "Connect GitHub";
  }
}

async function loadSession() {
  const value = await fetchJson("/admin/api/v1/session");
  if (
    value.apiVersion !== SESSION_VERSION ||
    !value.principal ||
    typeof value.principal.userId !== "string" ||
    typeof value.principal.displayEmail !== "string" ||
    typeof value.csrf !== "string"
  ) {
    throw new Error("browser session contract mismatch");
  }
  principalEmail.textContent = value.principal.displayEmail;
  canonicalUser.textContent = value.principal.userId;
  csrf = value.csrf;
}

async function fetchConnectionStatus() {
  clearError();
  const value = await fetchJson("/admin/api/v1/connections/github");
  if (
    value.apiVersion !== CONNECTIONS_VERSION ||
    value.provider !== "github" ||
    !value.status ||
    !["disconnected", "connecting", "connected", "reauth_required", "unavailable"].includes(
      value.status.phase
    ) ||
    !Array.isArray(value.status.scopesRequired) ||
    !Array.isArray(value.status.scopesGranted) ||
    !Array.isArray(value.status.scopesMissing)
  ) {
    throw new Error("provider connection contract mismatch");
  }
  return value.status;
}

function applyConnectionStatus(status) {
  renderConnection(status);
  if (!callbackStatus.hidden && status.phase === "connected") {
    callbackStatus.textContent = "GitHub connected.";
  }
}

async function loadConnection() {
  applyConnectionStatus(await fetchConnectionStatus());
}

async function bootstrapRuntime() {
  runtimeStatus.hidden = false;
  runtimeStatus.textContent = "Preparing preview runtime…";
  try {
    const value = await fetchJson("/admin/api/v1/fast-track/connections/runtime", {
      method: "POST",
      headers: mutationHeaders(),
      body: "{}",
    });
    if (
      value.runtimeId !== FAST_TRACK_RUNTIME_ID ||
      typeof value.status !== "string" ||
      !FAST_TRACK_RUNTIME_PHASES.has(value.status)
    ) {
      throw new Error("preview runtime response mismatch");
    }
    runtimeStatus.textContent = `Preview runtime ${value.status}.`;
    return value.status;
  } catch (_error) {
    runtimeStatus.textContent = "Preview runtime unavailable.";
    throw new Error("preview runtime unavailable");
  }
}

function renderBffStage(runtimePhase, stage) {
  const safeStage = FAST_TRACK_BFF_STAGE_LABELS.has(stage) ? stage : "unclassified";
  const label = FAST_TRACK_BFF_STAGE_LABELS.get(safeStage) || "waiting for connection path";
  runtimeStatus.dataset.bffStage = safeStage;
  runtimeStatus.textContent = `Preview runtime ${runtimePhase}. ${label}.`;
}

function renderPreviewChecking(runtimePhase, stage) {
  providerCard.dataset.phase = "loading";
  githubStatus.textContent = "Checking…";
  githubSummary.textContent = "Waiting for the governed preview runtime and connection path.";
  connectGithub.hidden = false;
  connectGithub.disabled = true;
  disconnectGithub.hidden = true;
  retryGithub.hidden = true;
  renderBffStage(runtimePhase, stage);
}

function pollDelay() {
  return new Promise((resolve) => window.setTimeout(resolve, FAST_TRACK_STATUS_POLL_INTERVAL_MS));
}

async function loadConnectionWithPreviewPolling(initialRuntimePhase) {
  const deadline = Date.now() + FAST_TRACK_STATUS_POLL_DEADLINE_MS;
  let runtimePhase = initialRuntimePhase;
  let consecutiveReadyChecks = 0;
  for (;;) {
    try {
      const status = await fetchConnectionStatus();
      if (runtimePhase === "running") {
        consecutiveReadyChecks += 1;
      } else {
        consecutiveReadyChecks = 0;
      }
      if (consecutiveReadyChecks >= 2) {
        renderConnection(status);
        if (!callbackStatus.hidden && status.phase === "connected") {
          callbackStatus.textContent = "GitHub connected.";
        }
        runtimeStatus.dataset.bffStage = "ready";
        runtimeStatus.textContent = `Preview runtime ${runtimePhase}. Connection path ready.`;
        return;
      }
      renderPreviewChecking(runtimePhase, null);
    } catch (error) {
      consecutiveReadyChecks = 0;
      const requestStatus = error && error.requestStatus;
      const stage = error && error.bffStage;
      renderPreviewChecking(runtimePhase, stage);
      if (
        requestStatus === 401 ||
        requestStatus === 403 ||
        requestStatus !== 503 ||
        !FAST_TRACK_RETRYABLE_BFF_STAGES.has(stage) ||
        !FAST_TRACK_POLLABLE_RUNTIME_PHASES.has(runtimePhase)
      ) {
        throw error;
      }
    }
    if (
      !FAST_TRACK_POLLABLE_RUNTIME_PHASES.has(runtimePhase) ||
      Date.now() + FAST_TRACK_STATUS_POLL_INTERVAL_MS > deadline
    ) {
      throw new Error("preview readiness deadline reached");
    }
    await pollDelay();
    runtimePhase = await bootstrapRuntime();
  }
}

async function startConnection() {
  setBusy(true);
  clearError();
  try {
    const value = await fetchJson("/admin/api/v1/connections/github/start", {
      method: "POST",
      headers: mutationHeaders(),
      body: "{}",
    });
    if (
      value.apiVersion !== CONNECTIONS_VERSION ||
      value.provider !== "github" ||
      typeof value.authorizationUrl !== "string" ||
      !isAllowedAuthorizationUrl(value.authorizationUrl)
    ) {
      throw new Error("provider authorization response mismatch");
    }
    window.location.assign(value.authorizationUrl);
  } catch (_error) {
    showError("GitHub consent could not be started. No credential was stored.");
    setBusy(false);
  }
}

async function disconnectConnection() {
  setBusy(true);
  clearError();
  try {
    const response = await fetch("/admin/api/v1/connections/github/disconnect", {
      method: "POST",
      cache: "no-store",
      credentials: "same-origin",
      headers: mutationHeaders(),
      body: JSON.stringify({ confirm: true }),
    });
    if (response.status !== 204) {
      throw new Error("provider disconnect rejected");
    }
    disconnectDialog.close();
    await loadConnection();
    callbackStatus.textContent = "";
    callbackStatus.hidden = true;
  } catch (_error) {
    disconnectDialog.close();
    showError("GitHub could not be disconnected. Refresh the status before retrying.");
  } finally {
    setBusy(false);
  }
}

function announceCallback() {
  if (window.location.hash === "#github-connected") {
    callbackStatus.textContent = "GitHub consent returned. Refreshing authoritative status…";
    callbackStatus.hidden = false;
    window.history.replaceState(null, "", "/admin/connections");
  } else if (window.location.hash === "#github-denied") {
    callbackStatus.textContent = "GitHub consent was not completed. No credential was stored.";
    callbackStatus.hidden = false;
    window.history.replaceState(null, "", "/admin/connections");
  }
}

connectGithub.addEventListener("click", () => void startConnection());
retryGithub.addEventListener("click", () => void (async () => {
  try {
    if (fastTrackRuntimeBootstrap) {
      const runtimePhase = await bootstrapRuntime();
      await loadConnectionWithPreviewPolling(runtimePhase);
    } else {
      await loadConnection();
    }
  } catch (_error) {
    renderConnection({
      phase: "unavailable",
      accountEmail: null,
      expiresAt: null,
      scopesRequired: [],
      scopesGranted: [],
      scopesMissing: [],
    });
  }
})());
disconnectGithub.addEventListener("click", () => disconnectDialog.showModal());
confirmDisconnect.addEventListener("click", () => void disconnectConnection());

async function initialize() {
  announceCallback();
  try {
    await loadSession();
    if (fastTrackRuntimeBootstrap) {
      const runtimePhase = await bootstrapRuntime();
      await loadConnectionWithPreviewPolling(runtimePhase);
    } else {
      await loadConnection();
    }
  } catch (_error) {
    providerCard.dataset.phase = "unavailable";
    githubStatus.textContent = "Unavailable";
    githubSummary.textContent = "Your session or provider status could not be verified.";
    retryGithub.hidden = false;
    connectGithub.hidden = true;
    showError("Connections could not be loaded. Sign in again or retry the status check.");
  }
}

void initialize();
