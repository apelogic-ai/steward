// TEMPORARY verification for steward #77 (draft PR only; remove before merge).
// Bundled mcp-gw fix branch fix/sandbox-helper-standalone-environment @ 60d4c86:
// servers/governed-cli/src/{sandbox-helper,process-environment,shim}.ts, compiled in CI as the release does.
// @bun
// servers/governed-cli/src/process-environment.ts
import { readFileSync } from "fs";
var readOwnEnviron = () => readFileSync("/proc/self/environ");
function sandboxHelperEnvironment(runtime, readEnviron = readOwnEnviron) {
  if (Object.keys(runtime).length > 0)
    return runtime;
  let environ;
  try {
    environ = readEnviron();
  } catch {
    return {};
  }
  const env = {};
  for (const entry of new TextDecoder().decode(environ).split("\x00")) {
    const separator = entry.indexOf("=");
    if (separator <= 0)
      continue;
    env[entry.slice(0, separator)] = entry.slice(separator + 1);
  }
  return env;
}

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
async function runSandboxHelper(options) {
  const stderr = options.stderr ?? process.stderr;
  const fail = () => {
    stderr.write(`Sandbox helper configuration failed
`);
    return HELPER_FAILURE_EXIT_CODE;
  };
  try {
    const invokedAs = basename(options.argv0);
    const operationIndex = options.args.indexOf("--operation");
    if (operationIndex > 0)
      return fail();
    let command = invokedAs;
    let args = options.args;
    if (operationIndex === 0) {
      const operation = options.args[1];
      if (operation !== "kubectl" && operation !== "remote-shell")
        return fail();
      if ((invokedAs === "kubectl" || invokedAs === "remote-shell") && invokedAs !== operation) {
        return fail();
      }
      command = operation;
      args = options.args.slice(2);
    }
    const token = required(options.env, "MCP_GW_RUN_TOKEN");
    if (command === "remote-shell") {
      if (args.length < 3 || args[1] !== "--")
        return fail();
      const target = args[0];
      const remoteCommand = args.slice(2).join(" ");
      if (!target?.trim() || !remoteCommand.trim())
        return fail();
      return await runRemoteShellShim({
        target,
        command: remoteCommand,
        endpoint: endpoint(options.env, "MCP_GW_SHELL_URL"),
        token,
        fetch: options.fetch,
        stdout: options.stdout,
        stderr: options.stderr
      });
    }
    if (!command || command === "mcp-gw-sandbox-helper")
      return fail();
    return await runCliShim({
      cli: command,
      args,
      endpoint: endpoint(options.env, "MCP_GW_CLI_URL"),
      token,
      fetch: options.fetch,
      stdout: options.stdout,
      stderr: options.stderr
    });
  } catch {
    return fail();
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
function required(env, name) {
  const value = env[name]?.trim();
  if (!value)
    throw new Error(`Missing ${name}`);
  return value;
}
function endpoint(env, name) {
  const value = required(env, name);
  const url = new URL(value);
  const loopback = url.protocol === "http:" && (url.hostname === "127.0.0.1" || url.hostname === "[::1]");
  if (url.protocol !== "https:" && !loopback)
    throw new Error(`${name} must use HTTPS`);
  return value;
}
function isRecord(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

// servers/governed-cli/src/sandbox-helper.ts
process.exitCode = await runSandboxHelper({
  argv0: process.argv0,
  args: process.argv.slice(2),
  env: sandboxHelperEnvironment(process.env)
});
