"use strict";

const SESSION_VERSION = "steward.browser-session/v1";
const CONNECTIONS_VERSION = "steward.connections/v1";

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

async function fetchJson(path, options = {}) {
  const response = await fetch(path, {
    cache: "no-store",
    credentials: "same-origin",
    headers: { Accept: "application/json", ...(options.headers || {}) },
    ...options,
  });
  if (!response.ok) {
    throw new Error(`request rejected with status ${response.status}`);
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

async function loadConnection() {
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
  renderConnection(value.status);
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
      !value.authorizationUrl.startsWith("https://")
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
retryGithub.addEventListener("click", () => void loadConnection().catch(() => {
  renderConnection({
    phase: "unavailable",
    accountEmail: null,
    expiresAt: null,
    scopesRequired: [],
    scopesGranted: [],
    scopesMissing: [],
  });
}));
disconnectGithub.addEventListener("click", () => disconnectDialog.showModal());
confirmDisconnect.addEventListener("click", () => void disconnectConnection());

async function initialize() {
  announceCallback();
  try {
    await loadSession();
    await loadConnection();
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
