import { spawn } from "node:child_process";
import { once } from "node:events";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { expect, test } from "@playwright/test";

const repository = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
let demo;
let origin;

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

async function guardedPage(browser, viewport) {
  const context = await browser.newContext({ viewport });
  await context.addInitScript(() => {
    const forbiddenStorageAccess = () => {
      throw new Error("Steward browser UI must not use browser storage");
    };
    for (const method of ["clear", "getItem", "key", "removeItem", "setItem"]) {
      Object.defineProperty(Storage.prototype, method, {
        configurable: true,
        value: forbiddenStorageAccess,
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

test.beforeAll(async () => {
  demo = await startLoopbackDemo();
  origin = demo.origin;
});

test.afterAll(async () => {
  await stopLoopbackDemo(demo?.child);
});

test("user can sign in, navigate their workspace, and connect then disconnect GitHub", async ({ browser }) => {
  const session = await guardedPage(browser, { width: 1440, height: 900 });
  try {
    const { page } = session;
    await signIn(page);

    const navigation = page.getByRole("navigation", { name: "Steward user workspace" });
    await expect(navigation).toContainText("My Envelopes");
    await expect(navigation).toContainText("New envelope");
    await expect(navigation).toContainText("My Runs");
    await expect(navigation).toContainText("Connections");

    await expect(page.locator("#github-status")).toHaveText("Not connected");
    await page.getByRole("button", { name: "Connect GitHub" }).click();
    await expect(page).toHaveURL(`${origin}/admin/connections`);
    await expect(page.locator("#github-status")).toHaveText("Connected");
    await page.getByRole("button", { name: "Disconnect", exact: true }).click();
    const disconnectDialog = page.getByRole("dialog", { name: "Disconnect GitHub?" });
    await expect(disconnectDialog).toBeVisible();
    await disconnectDialog.getByRole("button", { name: "Disconnect GitHub", exact: true }).click();
    await expect(page.locator("#github-status")).toHaveText("Not connected");

    for (const [label, route, heading] of [
      ["My Envelopes", "/app/envelopes", "My Envelopes"],
      ["New envelope", "/app/envelopes/new", "New envelope"],
      ["My Runs", "/app/runs", "My Runs"],
    ]) {
      await page.getByRole("link", { name: label, exact: true }).click();
      await expect(page).toHaveURL(`${origin}${route}`);
      await expect(page.getByRole("heading", { name: heading })).toBeVisible();
    }
  } finally {
    await closeGuardedPage(session);
  }
});

test("narrow user navigation remains available without horizontal overflow", async ({ browser }) => {
  const session = await guardedPage(browser, { width: 390, height: 844 });
  try {
    const { page } = session;
    await signIn(page);
    const navigation = page.getByRole("navigation", { name: "Steward user workspace" });
    await expect(navigation).toBeVisible();
    await expect
      .poll(() => page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth))
      .toBe(true);
    for (const label of ["My Envelopes", "New envelope", "My Runs", "Connections"]) {
      await expect(navigation.getByRole("link", { name: label, exact: true })).toBeVisible();
    }
  } finally {
    await closeGuardedPage(session);
  }
});
