import { spawn } from "node:child_process";
import { once } from "node:events";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { chromium, expect, test } from "@playwright/test";

const repository = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const examplesDirectory = path.join(repository, "target", "debug", "examples");
const executableSuffix = process.platform === "win32" ? ".exe" : "";
let demo;
let origin;
let adminDemo;
let adminOrigin;
let sharedBrowser;
const DEMO_STARTUP_TIMEOUT_MS = 120_000;

function exampleBinary(name) {
  return path.join(examplesDirectory, `${name}${executableSuffix}`);
}

function assertDemoIsRunning(demo, name) {
  if (demo?.exit) {
    throw new Error(`${name} exited unexpectedly (${demo.exit.code ?? demo.exit.signal}):\n${demo.output()}`);
  }
}

function startLoopbackDemo() {
  return new Promise((resolve, reject) => {
    const child = spawn(
      exampleBinary("user-envelope-demo"),
      ["--bind", "127.0.0.1:0"],
      // The xtask gate builds examples first. Give each direct demo process its
      // own session so teardown cannot leave it holding a port after the test
      // runner exits.
      { cwd: repository, stdio: ["ignore", "pipe", "pipe"], detached: process.platform !== "win32" },
    );
    let output = "";
    let settled = false;
    const timeout = setTimeout(() => {
      if (!settled) {
        settled = true;
        child.kill("SIGINT");
        reject(new Error(`loopback Steward demo did not become ready:\n${output}`));
      }
    }, DEMO_STARTUP_TIMEOUT_MS);
    const inspect = (chunk) => {
      output = `${output}${chunk}`.slice(-16_384);
      const match = output.match(/Steward envelope localhost demo: (http:\/\/127\.0\.0\.1:\d+)/);
      if (match && !settled) {
        settled = true;
        clearTimeout(timeout);
        resolve({
          child,
          origin: match[1],
          output: () => output,
          get exit() { return child.exitCode === null && child.signalCode === null ? null : { code: child.exitCode, signal: child.signalCode }; },
        });
      }
    };
    child.stdout.on("data", inspect);
    child.stderr.on("data", inspect);
    child.once("error", (error) => {
      if (!settled) {
        settled = true;
        clearTimeout(timeout);
        reject(error);
      }
    });
    child.once("exit", (code, signal) => {
      if (!settled) {
        settled = true;
        clearTimeout(timeout);
        reject(new Error(`loopback Steward demo exited before readiness (${code ?? signal}):\n${output}`));
      }
    });
  });
}

function startLoopbackAdminDemo() {
  return new Promise((resolve, reject) => {
    const child = spawn(
      exampleBinary("admin-dashboard-demo"),
      ["--mode", "oidc-admin", "--bind", "127.0.0.1:0"],
      { cwd: repository, stdio: ["ignore", "pipe", "pipe"], detached: process.platform !== "win32" },
    );
    let output = "";
    let settled = false;
    const timeout = setTimeout(() => {
      if (!settled) {
        settled = true;
        child.kill("SIGINT");
        reject(new Error(`loopback Steward admin demo did not become ready:\n${output}`));
      }
    }, DEMO_STARTUP_TIMEOUT_MS);
    const inspect = (chunk) => {
      output = `${output}${chunk}`.slice(-16_384);
      const match = output.match(/Steward localhost demo: (http:\/\/127\.0\.0\.1:\d+)\/admin\/sign-in/);
      if (match && !settled) {
        settled = true;
        clearTimeout(timeout);
        resolve({
          child,
          origin: match[1],
          output: () => output,
          get exit() { return child.exitCode === null && child.signalCode === null ? null : { code: child.exitCode, signal: child.signalCode }; },
        });
      }
    };
    child.stdout.on("data", inspect);
    child.stderr.on("data", inspect);
    child.once("error", (error) => {
      if (!settled) {
        settled = true;
        clearTimeout(timeout);
        reject(error);
      }
    });
    child.once("exit", (code, signal) => {
      if (!settled) {
        settled = true;
        clearTimeout(timeout);
        reject(new Error(`loopback Steward admin demo exited before readiness (${code ?? signal}):\n${output}`));
      }
    });
  });
}

async function stopLoopbackDemo(child) {
  if (!child || child.exitCode !== null) {
    return;
  }
  const terminate = (signal) => {
    if (process.platform !== "win32") {
      try {
        process.kill(-child.pid, signal);
        return;
      } catch (error) {
        if (error.code !== "ESRCH") throw error;
      }
    }
    child.kill(signal);
  };
  terminate("SIGINT");
  await Promise.race([
    once(child, "exit"),
    new Promise((resolve) => setTimeout(resolve, 10_000)),
  ]);
  if (child.exitCode === null) {
    terminate("SIGKILL");
    await once(child, "exit");
  }
}

async function guardedPage(browser, viewport) {
  const context = await browser.newContext({ viewport });
  await context.addInitScript(() => {
    const allowedPreference = (key) => typeof key === "string" && key.startsWith("steward.ui.envelope-accordion.");
    for (const method of ["getItem", "removeItem", "setItem"]) {
      const original = Storage.prototype[method];
      Object.defineProperty(Storage.prototype, method, {
        configurable: true,
        value(...args) {
          if (!allowedPreference(args[0])) throw new Error("Steward browser UI may persist only envelope accordion preferences");
          return original.apply(this, args);
        },
      });
    }
    for (const method of ["clear", "key"]) {
      Object.defineProperty(Storage.prototype, method, {
        configurable: true,
        value() { throw new Error("Steward browser UI must not enumerate or clear browser storage"); },
      });
    }
  });
  const page = await context.newPage();
  const consoleErrors = [];
  page.on("console", (message) => {
    if (message.type() === "error") {
      consoleErrors.push(message.text());
    }
  });
  page.on("pageerror", (error) => consoleErrors.push(error.message));
  return { context, page, consoleErrors };
}

async function closeGuardedPage(session) {
  try {
    expect(session.consoleErrors, "browser console errors must fail the UI gate").toEqual([]);
  } finally {
    await session.context.close();
  }
}

async function accordionIndicator(details) {
  return details.locator("summary").evaluate(
    (summary) => getComputedStyle(summary, "::before").content.replaceAll('"', ""),
  );
}

async function signIn(page) {
  await page.goto(`${origin}/admin/sign-in`);
  await expect(page.getByRole("heading", { name: "Sign in to Steward" })).toBeVisible();
  await page.getByRole("link", { name: "Continue with Google" }).click();
  await expect(page).toHaveURL(`${origin}/envelopes`);
}

test.beforeAll(async () => {
  demo = await startLoopbackDemo();
  origin = demo.origin;
  adminDemo = await startLoopbackAdminDemo();
  adminOrigin = adminDemo.origin;
  sharedBrowser = await chromium.launch();
});

test.afterAll(async () => {
  try {
    await sharedBrowser?.close();
  } finally {
    await stopLoopbackDemo(demo?.child);
    await stopLoopbackDemo(adminDemo?.child);
  }
});

test.beforeEach(() => {
  assertDemoIsRunning(demo, "loopback Steward envelope demo");
  assertDemoIsRunning(adminDemo, "loopback Steward admin demo");
});

test.afterEach(() => {
  assertDemoIsRunning(demo, "loopback Steward envelope demo");
  assertDemoIsRunning(adminDemo, "loopback Steward admin demo");
});

test("dual-role administrator can switch presentation, while developer cannot enter it", async () => {
  const userSession = await guardedPage(sharedBrowser, { width: 1440, height: 900 });
  const adminSession = await guardedPage(sharedBrowser, { width: 1440, height: 900 });
  try {
    await signIn(userSession.page);
    const forbiddenWorkspace = await userSession.page.request.get(`${origin}/admin/workspace`);
    expect(forbiddenWorkspace.status()).toBe(403);

    const { page } = adminSession;
    await page.goto(`${adminOrigin}/admin/sign-in`);
    await page.getByRole("link", { name: "Continue with Google" }).click();
    await expect(page).toHaveURL(`${adminOrigin}/envelopes`);
    const workspace = page.getByRole("combobox", { name: "Workspace presentation" });
    await expect(workspace).toBeVisible();
    await expect(workspace).toHaveValue("developer");
    await workspace.selectOption("admin");
    await expect(page).toHaveURL(`${adminOrigin}/admin/workspace`);
    await expect(page.getByRole("heading", { name: "Admin" })).toBeVisible();
    await expect(workspace).toHaveValue("admin");
    await expect(page.locator("#admin-runs-list")).toContainText("No entries.");

    await page.getByRole("link", { name: "Connections", exact: true }).click();
    await expect(page).toHaveURL(`${adminOrigin}/admin/connections`);
    await expect(page.getByRole("combobox", { name: "Workspace presentation" })).toBeVisible();
  } finally {
    await closeGuardedPage(userSession);
    await closeGuardedPage(adminSession);
  }
});

test("user can sign in, navigate shared top navigation, and connect then disconnect GitHub", async () => {
  const session = await guardedPage(sharedBrowser, { width: 1440, height: 900 });
  try {
    const { page } = session;
    await signIn(page);

    const navigation = page.getByRole("navigation", { name: "Steward primary navigation" });
    for (const label of ["Envelopes", "Runs", "Connections", "Settings"]) {
      await expect(navigation.getByRole("link", { name: label, exact: true })).toBeVisible();
    }
    await expect(page.locator("#signed-in-email")).toHaveText("alice@example.com");

    await navigation.getByRole("link", { name: "Connections", exact: true }).click();
    await expect(page).toHaveURL(`${origin}/admin/connections`);
    await expect(page.getByRole("heading", { name: "Connections" })).toBeVisible();
    await expect(page.locator("#signed-in-email")).toHaveText("alice@example.com");

    await expect(page.locator("#github-status")).toHaveText("Not connected");
    await page.getByRole("button", { name: "Connect GitHub" }).click();
    await expect(page).toHaveURL(`${origin}/admin/connections`);
    await expect(page.locator("#github-status")).toHaveText("Connected");
    await page.getByRole("button", { name: "Disconnect", exact: true }).click();
    const disconnectDialog = page.getByRole("dialog", { name: "Disconnect GitHub?" });
    await expect(disconnectDialog).toBeVisible();
    await disconnectDialog.getByRole("button", { name: "Disconnect GitHub", exact: true }).click();
    await expect(page.locator("#github-status")).toHaveText("Not connected");

    await navigation.getByRole("link", { name: "Envelopes", exact: true }).click();
    await expect(page).toHaveURL(`${origin}/envelopes`);
    await navigation.getByRole("link", { name: "Runs", exact: true }).click();
    await expect(page).toHaveURL(`${origin}/runs`);
    await expect(page).toHaveTitle("Runs · Steward");
    await expect(page.getByRole("heading", { name: "Runs" })).toBeVisible();
    await expect(page.locator("#runs-list")).toContainText("No entries.");
    await navigation.getByRole("link", { name: "Envelopes", exact: true }).click();
    await expect(page).toHaveURL(`${origin}/envelopes`);
    for (const name of ["Templates", "Drafts", "Approved", "In Review"]) {
      await expect(page.locator(`details[data-accordion=${name.toLowerCase().replace(" ", "-")}]`)).toHaveJSProperty("open", false);
    }
    const templates = page.locator("details[data-accordion=templates]");
    await expect(templates.locator("summary")).toHaveCSS("list-style-type", "none");
    await expect.poll(() => accordionIndicator(templates)).toBe("›");
    await expect(page.getByText("Admin editable")).toHaveCount(0);
    await expect(page.getByText("User only")).toHaveCount(0);
    await expect(page.getByText("User and admin")).toHaveCount(0);
    await templates.locator("summary").click();
    await expect(templates).toHaveJSProperty("open", true);
    await expect.poll(() => accordionIndicator(templates)).toBe("⌄");
    await expect(templates).toContainText("Engineer · revision 3");
    await page.reload();
    await expect(templates).toHaveJSProperty("open", true);
    const newEnvelope = page.getByRole("link", { name: "New envelope" });
    await expect(newEnvelope).toHaveCSS("text-decoration-line", "none");
    await newEnvelope.click();
    await expect(page.getByRole("heading", { name: "New envelope" })).toBeVisible();
    await expect(navigation.getByRole("link", { name: "Envelopes", exact: true })).toHaveAttribute("aria-current", "page");
    await expect(page.getByRole("combobox", { name: "Template" })).toHaveValue("engineer");
    await expect(page.getByRole("combobox", { name: "Models" })).toHaveValue("openai/gpt-5.4");
    await expect(page.getByRole("combobox", { name: "Tools" })).toHaveValue("github:repository:get_file_contents");
    await expect(page.getByRole("combobox", { name: "Required connection" })).toHaveValue("GitHub · connected");
    await expect(page.getByRole("combobox", { name: "Runner platform" })).toHaveValue("linux");
    await expect(page.getByRole("combobox", { name: "Runner memory" })).toHaveValue("1Gi");
    await expect(page.getByRole("combobox", { name: "Runner compute" })).toHaveValue("500m");
    await expect(page.getByRole("combobox", { name: "Runner storage" })).toHaveValue("5Gi");
    await page.getByRole("button", { name: "Request envelope" }).click();
    await expect(page).toHaveURL(/\/envelopes\/[0-9a-f-]{36}$/);
    await expect(page.getByRole("heading", { name: "Envelope form" })).toBeVisible();
    await expect(page.locator("#envelope-detail")).toContainText("provisioned");
    await expect(page.locator("#requested-tools")).toContainText("github:repository:get_file_contents");
    await expect(page.locator("#requested-models")).toContainText("openai/gpt-5.4");
    await expect(page.locator("#approved-tools")).toContainText("github:repository:get_file_contents");
    await expect(page.locator("#requested-platform")).toContainText("linux");
    await expect(page.locator("#approved-platform")).toContainText("linux");
    await expect(page.locator("#requested-memory")).toContainText("1Gi");
    await expect(page.locator("#approved-memory")).toContainText("1Gi");
    await page.getByRole("link", { name: "Recent runs" }).click();
    await expect(page.getByRole("heading", { name: "Recent runs" })).toBeVisible();
    await expect(page.locator("#envelope-runs-list")).toContainText("No entries.");
    await page.getByRole("link", { name: "Settings", exact: true }).click();
    await expect(page).toHaveURL(`${origin}/settings`);
    await expect(page).toHaveTitle("Settings · Steward");
    await expect(page.getByRole("heading", { name: "Settings" })).toBeVisible();
  } finally {
    await closeGuardedPage(session);
  }
});

test("narrow user navigation remains available without horizontal overflow", async () => {
  const session = await guardedPage(sharedBrowser, { width: 390, height: 844 });
  try {
    const { page } = session;
    await signIn(page);
    const navigation = page.getByRole("navigation", { name: "Steward primary navigation" });
    await expect(navigation).toBeVisible();
    await expect
      .poll(() => page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth))
      .toBe(true);
    for (const label of ["Envelopes", "Runs", "Connections", "Settings"]) {
      await expect(navigation.getByRole("link", { name: label, exact: true })).toBeVisible();
    }
  } finally {
    await closeGuardedPage(session);
  }
});
