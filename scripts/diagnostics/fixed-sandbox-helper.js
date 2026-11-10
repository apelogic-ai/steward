// TEMPORARY verification for steward #77 (draft PR only; remove before merge).
// Bundled mcp-gw fix branch fix/sandbox-helper-explicit-config @ 43030f1:
// servers/governed-cli/src/{sandbox-helper,shim}.ts, compiled in CI as the release does.
// @bun
// servers/governed-cli/src/shim.ts
import { basename } from "path";
var HELPER_FAILURE_EXIT_CODE = 125;
async function runCliShim(options) {
  return runRequest({
    endpoint: options.endpoint,
    token: options.token,
    body: { cli: options.cli, args: options.args },
    fetch: options.fetch,
    stdout: options.stdout,
    stderr: options.stderr
  }, `CLI request failed
`);
}
async function runRemoteShellShim(options) {
  return runRequest({
    endpoint: options.endpoint,
    token: options.token,
    body: { target: options.target, command: options.command },
    fetch: options.fetch,
    stdout: options.stdout,
    stderr: options.stderr
  }, `Remote shell request failed
`);
}

class HelperConfigurationError extends Error {
}
var HELPER_OPTIONS = {
  "--run-token": "MCP_GW_RUN_TOKEN",
  "--cli-url": "MCP_GW_CLI_URL",
  "--shell-url": "MCP_GW_SHELL_URL"
};
function leadingOptions(args) {
  const overrides = {};
  let index = 0;
  while (index < args.length && Object.hasOwn(HELPER_OPTIONS, args[index] ?? "")) {
    const name = HELPER_OPTIONS[args[index]];
    const value = args[index + 1];
    if (value === undefined || value.startsWith("--") || Object.hasOwn(overrides, name)) {
      throw new HelperConfigurationError("invalid helper option");
    }
    overrides[name] = value;
    index += 2;
  }
  if (index > 0 && args[index] !== "--operation") {
    throw new HelperConfigurationError("invalid helper option");
  }
  return { overrides, rest: args.slice(index) };
}
async function runSandboxHelper(options) {
  const stderr = options.stderr ?? process.stderr;
  const fail = (reason) => {
    stderr.write(`Sandbox helper configuration failed: ${reason}
`);
    return HELPER_FAILURE_EXIT_CODE;
  };
  try {
    const invokedAs = basename(options.argv0);
    const { overrides, rest } = leadingOptions(options.args);
    const env = { ...options.env, ...overrides };
    const operationIndex = rest.indexOf("--operation");
    if (operationIndex > 0)
      return fail("invalid operation");
    let command = invokedAs;
    let args = rest;
    if (operationIndex === 0) {
      const operation = rest[1];
      if (operation !== "kubectl" && operation !== "remote-shell")
        return fail("invalid operation");
      if ((invokedAs === "kubectl" || invokedAs === "remote-shell") && invokedAs !== operation) {
        return fail("invalid operation");
      }
      command = operation;
      args = rest.slice(2);
    }
    const token = env.MCP_GW_RUN_TOKEN?.trim();
    if (!token)
      return fail("missing run token");
    if (command === "remote-shell") {
      if (args.length < 3 || args[1] !== "--")
        return fail("invalid remote-shell arguments");
      const target = args[0];
      const remoteCommand = args.slice(2).join(" ");
      if (!target?.trim() || !remoteCommand.trim())
        return fail("invalid remote-shell arguments");
      return await runRemoteShellShim({
        target,
        command: remoteCommand,
        endpoint: endpoint(env, "MCP_GW_SHELL_URL"),
        token,
        fetch: options.fetch,
        stdout: options.stdout,
        stderr: options.stderr
      });
    }
    if (!command || command === "mcp-gw-sandbox-helper")
      return fail("invalid operation");
    return await runCliShim({
      cli: command,
      args,
      endpoint: endpoint(env, "MCP_GW_CLI_URL"),
      token,
      fetch: options.fetch,
      stdout: options.stdout,
      stderr: options.stderr
    });
  } catch (error) {
    return fail(error instanceof HelperConfigurationError ? error.message : "unexpected error");
  }
}
async function runRequest(options, failureMessage) {
  const stdout = options.stdout ?? process.stdout;
  const stderr = options.stderr ?? process.stderr;
  const fail = () => {
    stderr.write(failureMessage);
    return HELPER_FAILURE_EXIT_CODE;
  };
  let response;
  try {
    response = await (options.fetch ?? fetch)(options.endpoint, {
      method: "POST",
      headers: {
        authorization: `Bearer ${options.token}`,
        "content-type": "application/json"
      },
      body: JSON.stringify(options.body),
      redirect: "error"
    });
  } catch {
    return fail();
  }
  if (!response.ok || !response.body)
    return fail();
  const reader = response.body.getReader();
  const decoder = new TextDecoder;
  let buffered = "";
  let exitCode;
  try {
    for (;; ) {
      const result = await reader.read();
      buffered += result.value ? decoder.decode(result.value, { stream: true }) : decoder.decode();
      let newline = buffered.indexOf(`
`);
      while (newline >= 0) {
        const line = buffered.slice(0, newline);
        buffered = buffered.slice(newline + 1);
        if (line)
          exitCode = consumeEvent(line, exitCode, stdout, stderr);
        newline = buffered.indexOf(`
`);
      }
      if (result.done)
        break;
    }
  } catch {
    return fail();
  }
  if (buffered) {
    try {
      exitCode = consumeEvent(buffered, exitCode, stdout, stderr);
    } catch {
      return fail();
    }
  }
  return exitCode ?? fail();
}
function consumeEvent(line, exitCode, stdout, stderr) {
  if (exitCode !== undefined)
    throw new Error("event followed exit");
  const event = JSON.parse(line);
  if (!isRecord(event) || typeof event.type !== "string")
    throw new Error("invalid event");
  if (event.type === "stdout" || event.type === "stderr") {
    if (typeof event.data !== "string")
      throw new Error("invalid output event");
    (event.type === "stdout" ? stdout : stderr).write(event.data);
    return;
  }
  if (event.type !== "exit" || !Number.isSafeInteger(event.exit_code) || event.exit_code < 0 || event.exit_code > 255) {
    throw new Error("invalid exit event");
  }
  return event.exit_code;
}
function endpoint(env, name) {
  const value = env[name]?.trim();
  if (!value)
    throw new HelperConfigurationError(`missing ${name}`);
  let url;
  try {
    url = new URL(value);
  } catch {
    throw new HelperConfigurationError(`${name} is not a valid URL`);
  }
  const loopback = url.protocol === "http:" && (url.hostname === "127.0.0.1" || url.hostname === "[::1]");
  if (url.protocol !== "https:" && !loopback) {
    throw new HelperConfigurationError(`${name} must use HTTPS`);
  }
  return value;
}
function isRecord(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

// servers/governed-cli/src/sandbox-helper.ts
process.exitCode = await runSandboxHelper({
  argv0: process.argv0,
  args: process.argv.slice(2),
  env: process.env
});
