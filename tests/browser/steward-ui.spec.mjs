import { spawn } from "node:child_process";
import { once } from "node:events";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { expect, test } from "@playwright/test";

const repository = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
let demo;
let origin;
let envelopeDemo;
let envelopeOrigin;

function startLoopbackDemo() {
  return new Promise((resolve, reject) => {
    const child = spawn(
      "cargo",
      [
        "run",
        "-p",
        "steward-apiserver",
        "--locked",
        "--features",
        "admin-demo",
        "--example",
        "admin-dashboard-demo",
        "--",
        "--mode",
        "oidc-user",
        "--bind",
        "127.0.0.1:0",
      ],
      { cwd: repository, stdio: ["ignore", "pipe", "pipe"] },
    );
    let output = "";
    let settled = false;
    const timeout = setTimeout(() => {
      if (!settled) {
        settled = true;
        child.kill("SIGINT");
        reject(new Error(`loopback Steward demo did not become ready:\n${output}`));
      }
    }, 30_000);
    const inspect = (chunk) => {
      output = `${output}${chunk}`.slice(-16_384);
      const match = output.match(/Steward localhost demo: (http:\/\/127\.0\.0\.1:\d+)/);
      if (match && !settled) {
        settled = true;
        clearTimeout(timeout);
        resolve({ child, origin: match[1], output });
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

async function stopLoopbackDemo(child) {
  if (!child || child.exitCode !== null) {
    return;
  }
  child.kill("SIGINT");
  await Promise.race([
    once(child, "exit"),
    new Promise((resolve) => setTimeout(resolve, 10_000)),
  ]);
  if (child.exitCode === null) {
    child.kill("SIGKILL");
    await once(child, "exit");
  }
}

function startEnvelopeDemo() {
  return new Promise((resolve, reject) => {
    const child = spawn(
      "cargo",
      ["run", "-p", "steward-apiserver", "--locked", "--features", "admin-demo", "--example", "user-envelope-demo", "--", "--bind", "127.0.0.1:0"],
      { cwd: repository, stdio: ["ignore", "pipe", "pipe"] },
    );
    let output = "";
    let settled = false;
    const timeout = setTimeout(() => {
      if (!settled) {
        settled = true;
        child.kill("SIGINT");
        reject(new Error(`loopback envelope demo did not become ready:\n${output}`));
      }
    }, 30_000);
    const inspect = (chunk) => {
      output = `${output}${chunk}`.slice(-16_384);
      const match = output.match(/Steward envelope localhost demo: (http:\/\/127\.0\.0\.1:\d+)/);
      if (match && !settled) {
        settled = true;
        clearTimeout(timeout);
        resolve({ child, origin: match[1], output });
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
        reject(new Error(`loopback envelope demo exited before readiness (${code ?? signal}):\n${output}`));
      }
    });
  });
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

async function signIn(page) {
  await page.goto(`${origin}/admin/sign-in`);
  await expect(page.getByRole("heading", { name: "Sign in to Steward" })).toBeVisible();
  await page.getByRole("link", { name: "Continue with Google" }).click();
  await expect(page).toHaveURL(`${origin}/admin/connections`);
  await expect(page.getByRole("heading", { name: "Connections" })).toBeVisible();
}

async function signInEnvelope(page) {
  await page.context().clearCookies();
  await page.goto(`${envelopeOrigin}/admin/sign-in`);
  await expect(page.getByRole("heading", { name: "Sign in to Steward" })).toBeVisible();
  await page.getByRole("link", { name: "Continue with Google" }).click();
  await expect(page).toHaveURL(`${envelopeOrigin}/envelopes`);
}

test.beforeAll(async () => {
  demo = await startLoopbackDemo();
  origin = demo.origin;
  envelopeDemo = await startEnvelopeDemo();
  envelopeOrigin = envelopeDemo.origin;
});

test.afterAll(async () => {
  await stopLoopbackDemo(demo?.child);
  await stopLoopbackDemo(envelopeDemo?.child);
});

test("user can sign in, navigate shared top navigation, and connect then disconnect GitHub", async ({ browser }) => {
  const session = await guardedPage(browser, { width: 1440, height: 900 });
  try {
    const { page } = session;
    await signIn(page);

    await expect(page.locator("#github-status")).toHaveText("Not connected");
    await page.getByRole("button", { name: "Connect GitHub" }).click();
    await expect(page).toHaveURL(`${origin}/admin/connections`);
    await expect(page.locator("#github-status")).toHaveText("Connected");
    await page.getByRole("button", { name: "Disconnect", exact: true }).click();
    const disconnectDialog = page.getByRole("dialog", { name: "Disconnect GitHub?" });
    await expect(disconnectDialog).toBeVisible();
    await disconnectDialog.getByRole("button", { name: "Disconnect GitHub", exact: true }).click();
    await expect(page.locator("#github-status")).toHaveText("Not connected");

    await signInEnvelope(page);
    const navigation = page.getByRole("navigation", { name: "Steward primary navigation" });
    for (const label of ["Envelopes", "Runs", "Connections", "Settings"]) {
      await expect(navigation.getByRole("link", { name: label, exact: true })).toBeVisible();
    }
    await expect(page.locator("#signed-in-email")).toHaveText("alice@example.com");
    await navigation.getByRole("link", { name: "Runs", exact: true }).click();
    await expect(page).toHaveURL(`${envelopeOrigin}/runs`);
    await expect(page.getByRole("heading", { name: "Runs" })).toBeVisible();
    await expect(page.locator("#runs-list")).toContainText("No runs are recorded for your identity.");
    await navigation.getByRole("link", { name: "Envelopes", exact: true }).click();
    await expect(page).toHaveURL(`${envelopeOrigin}/envelopes`);
    for (const name of ["Templates", "Drafts", "Approved", "In Review"]) {
      await expect(page.locator(`details[data-accordion=\"${name.toLowerCase().replace(" ", "-")}\"]`)).toHaveJSProperty("open", false);
    }
    const templates = page.locator("details[data-accordion=templates]");
    await templates.locator("summary").click();
    await expect(templates).toHaveJSProperty("open", true);
    await expect(templates).toContainText("Engineer · revision 3");
    await page.reload();
    await expect(templates).toHaveJSProperty("open", true);
    await page.goto(`${envelopeOrigin}/envelopes/new`);
    await expect(page.getByRole("heading", { name: "New envelope" })).toBeVisible();
    await expect(page.getByRole("combobox", { name: "Template" })).toHaveValue("engineer");
    await page.getByRole("button", { name: "Request envelope" }).click();
    await expect(page).toHaveURL(/\/envelopes\/[0-9a-f-]{36}$/);
    await expect(page.getByRole("heading", { name: "Envelope form" })).toBeVisible();
    await expect(page.locator("#envelope-detail")).toContainText("provisioned");
    await page.getByRole("link", { name: "Recent runs" }).click();
    await expect(page.getByRole("heading", { name: "Recent runs" })).toBeVisible();
    await expect(page.locator("#envelope-runs-list")).toContainText("No recent runs are recorded for this envelope instance.");
  } finally {
    await closeGuardedPage(session);
  }
});

test("narrow user navigation remains available without horizontal overflow", async ({ browser }) => {
  const session = await guardedPage(browser, { width: 390, height: 844 });
  try {
    const { page } = session;
    await signIn(page);
    await signInEnvelope(page);
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
