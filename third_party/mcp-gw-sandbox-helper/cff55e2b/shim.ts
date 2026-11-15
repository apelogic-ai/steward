import { readFileSync } from "node:fs";
import { basename } from "node:path";

const HELPER_FAILURE_EXIT_CODE = 125;

interface ShimWriter {
  write(data: string): unknown;
}

/** Egress settings passed explicitly when the helper can't see the sandbox environment. */
export interface HelperEgress {
  /** HTTP(S) proxy for non-loopback endpoints. */
  proxy?: string;
  /** PEM certificates trusted for the gateway connection, replacing the default roots. */
  ca?: string;
}

export interface RunCliShimOptions extends HelperEgress {
  cli: string;
  args: readonly string[];
  endpoint: string;
  token: string;
  fetch?: (input: string | URL | Request, init?: RequestInit) => Promise<Response>;
  stdout?: ShimWriter;
  stderr?: ShimWriter;
}

export interface RunRemoteShellShimOptions extends HelperEgress {
  target: string;
  command: string;
  endpoint: string;
  token: string;
  fetch?: (input: string | URL | Request, init?: RequestInit) => Promise<Response>;
  stdout?: ShimWriter;
  stderr?: ShimWriter;
}

export interface RunSandboxHelperOptions {
  argv0: string;
  args: readonly string[];
  env: Readonly<Record<string, string | undefined>>;
  fetch?: (input: string | URL | Request, init?: RequestInit) => Promise<Response>;
  stdout?: ShimWriter;
  stderr?: ShimWriter;
}

export async function runCliShim(options: RunCliShimOptions): Promise<number> {
  return runRequest(
    {
      endpoint: options.endpoint,
      token: options.token,
      body: { cli: options.cli, args: options.args },
      proxy: options.proxy,
      ca: options.ca,
      fetch: options.fetch,
      stdout: options.stdout,
      stderr: options.stderr,
    },
    "CLI request failed\n",
  );
}

export async function runRemoteShellShim(options: RunRemoteShellShimOptions): Promise<number> {
  return runRequest(
    {
      endpoint: options.endpoint,
      token: options.token,
      body: { target: options.target, command: options.command },
      proxy: options.proxy,
      ca: options.ca,
      fetch: options.fetch,
      stdout: options.stdout,
      stderr: options.stderr,
    },
    "Remote shell request failed\n",
  );
}

/** A configuration refusal with a fixed, non-secret reason. Never carries a value. */
class HelperConfigurationError extends Error {}

const HELPER_OPTIONS = {
  "--run-token": "MCP_GW_RUN_TOKEN",
  "--cli-url": "MCP_GW_CLI_URL",
  "--shell-url": "MCP_GW_SHELL_URL",
  "--proxy": "proxy",
  "--ca-file": "caFile",
} as const;

/**
 * Leading `--run-token`, `--cli-url`, `--shell-url`, `--proxy` and `--ca-file` options, accepted
 * only directly before `--operation`. Some process supervisors start the helper with an empty
 * environment, so wrappers pass the run-token placeholder, the command route, the sandbox egress
 * proxy and the CA of its TLS termination this way. Configuration options win over the
 * environment, which stays the fallback; without egress options the runtime's own proxy and
 * trust handling applies, unchanged.
 */
function leadingOptions(args: readonly string[]): {
  overrides: Record<string, string>;
  rest: readonly string[];
} {
  const overrides: Record<string, string> = {};
  let index = 0;
  while (index < args.length && Object.hasOwn(HELPER_OPTIONS, args[index] ?? "")) {
    const name = HELPER_OPTIONS[args[index] as keyof typeof HELPER_OPTIONS];
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

export async function runSandboxHelper(options: RunSandboxHelperOptions): Promise<number> {
  const stderr = options.stderr ?? process.stderr;
  const fail = (reason: string) => {
    stderr.write(`Sandbox helper configuration failed: ${reason}\n`);
    return HELPER_FAILURE_EXIT_CODE;
  };

  try {
    const invokedAs = basename(options.argv0);
    const {
      overrides: { proxy, caFile, ...configuration },
      rest,
    } = leadingOptions(options.args);
    const env = { ...options.env, ...configuration };
    const egress = helperEgress(proxy, caFile);
    const operationIndex = rest.indexOf("--operation");
    if (operationIndex > 0) return fail("invalid operation");

    let command = invokedAs;
    let args = rest;
    if (operationIndex === 0) {
      const operation = rest[1];
      if (operation !== "kubectl" && operation !== "remote-shell") return fail("invalid operation");
      if ((invokedAs === "kubectl" || invokedAs === "remote-shell") && invokedAs !== operation) {
        return fail("invalid operation");
      }
      command = operation;
      args = rest.slice(2);
    }

    const token = env.MCP_GW_RUN_TOKEN?.trim();
    if (!token) return fail("missing run token");
    if (command === "remote-shell") {
      if (args.length < 3 || args[1] !== "--") return fail("invalid remote-shell arguments");
      const target = args[0];
      const remoteCommand = args.slice(2).join(" ");
      if (!target?.trim() || !remoteCommand.trim()) return fail("invalid remote-shell arguments");
      return await runRemoteShellShim({
        target,
        command: remoteCommand,
        endpoint: endpoint(env, "MCP_GW_SHELL_URL"),
        token,
        ...egress,
        fetch: options.fetch,
        stdout: options.stdout,
        stderr: options.stderr,
      });
    }
    if (!command || command === "mcp-gw-sandbox-helper") return fail("invalid operation");
    return await runCliShim({
      cli: command,
      args,
      endpoint: endpoint(env, "MCP_GW_CLI_URL"),
      token,
      ...egress,
      fetch: options.fetch,
      stdout: options.stdout,
      stderr: options.stderr,
    });
  } catch (error) {
    return fail(error instanceof HelperConfigurationError ? error.message : "unexpected error");
  }
}

interface RunRequestOptions extends HelperEgress {
  endpoint: string;
  token: string;
  body: unknown;
  fetch?: (input: string | URL | Request, init?: RequestInit) => Promise<Response>;
  stdout?: ShimWriter;
  stderr?: ShimWriter;
}

async function runRequest(options: RunRequestOptions, failureMessage: string): Promise<number> {
  const stdout = options.stdout ?? process.stdout;
  const stderr = options.stderr ?? process.stderr;
  const fail = () => {
    stderr.write(failureMessage);
    return HELPER_FAILURE_EXIT_CODE;
  };

  let response: Response;
  try {
    const init: RequestInit & { proxy?: string; tls?: { ca: string } } = {
      method: "POST",
      headers: {
        authorization: `Bearer ${options.token}`,
        "content-type": "application/json",
      },
      body: JSON.stringify(options.body),
      redirect: "error",
    };
    // Loopback endpoints never go through the sandbox proxy (it lists them in NO_PROXY).
    if (options.proxy !== undefined && !isLoopback(new URL(options.endpoint))) {
      init.proxy = options.proxy;
    }
    if (options.ca !== undefined) init.tls = { ca: options.ca };
    response = await (options.fetch ?? fetch)(options.endpoint, init);
  } catch {
    return fail();
  }
  if (!response.ok || !response.body) return fail();

  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffered = "";
  let exitCode: number | undefined;
  try {
    for (;;) {
      const result = (await reader.read()) as { done: boolean; value?: Uint8Array };
      buffered += result.value ? decoder.decode(result.value, { stream: true }) : decoder.decode();
      let newline = buffered.indexOf("\n");
      while (newline >= 0) {
        const line = buffered.slice(0, newline);
        buffered = buffered.slice(newline + 1);
        if (line) exitCode = consumeEvent(line, exitCode, stdout, stderr);
        newline = buffered.indexOf("\n");
      }
      if (result.done) break;
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

function consumeEvent(
  line: string,
  exitCode: number | undefined,
  stdout: ShimWriter,
  stderr: ShimWriter,
): number | undefined {
  if (exitCode !== undefined) throw new Error("event followed exit");
  const event = JSON.parse(line) as unknown;
  if (!isRecord(event) || typeof event.type !== "string") throw new Error("invalid event");
  if (event.type === "stdout" || event.type === "stderr") {
    if (typeof event.data !== "string") throw new Error("invalid output event");
    (event.type === "stdout" ? stdout : stderr).write(event.data);
    return undefined;
  }
  if (
    event.type !== "exit" ||
    !Number.isSafeInteger(event.exit_code) ||
    (event.exit_code as number) < 0 ||
    (event.exit_code as number) > 255
  ) {
    throw new Error("invalid exit event");
  }
  return event.exit_code as number;
}

function endpoint(env: Readonly<Record<string, string | undefined>>, name: string): string {
  const value = env[name]?.trim();
  if (!value) throw new HelperConfigurationError(`missing ${name}`);
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    throw new HelperConfigurationError(`${name} is not a valid URL`);
  }
  if (url.protocol !== "https:" && !isLoopback(url)) {
    throw new HelperConfigurationError(`${name} must use HTTPS`);
  }
  return value;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isLoopback(url: URL): boolean {
  return url.protocol === "http:" && (url.hostname === "127.0.0.1" || url.hostname === "[::1]");
}

/** Validates explicit egress options; refusals name a fixed reason and never print a value. */
function helperEgress(proxy: string | undefined, caFile: string | undefined): HelperEgress {
  const egress: HelperEgress = {};
  if (proxy !== undefined) {
    let url: URL;
    try {
      url = new URL(proxy.trim());
    } catch {
      throw new HelperConfigurationError("invalid proxy URL");
    }
    if (url.protocol !== "http:" && url.protocol !== "https:") {
      throw new HelperConfigurationError("invalid proxy URL");
    }
    egress.proxy = proxy.trim();
  }
  if (caFile !== undefined) {
    let ca: string;
    try {
      ca = readFileSync(caFile.trim(), "utf8");
    } catch {
      throw new HelperConfigurationError("unreadable CA file");
    }
    if (!ca.includes("-----BEGIN CERTIFICATE-----")) {
      throw new HelperConfigurationError("invalid CA file");
    }
    egress.ca = ca;
  }
  return egress;
}
