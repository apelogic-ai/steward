type JsonObject = Record<string, unknown>;

const encoder = new TextEncoder();
const declaredOutput = "Repository review completed by codex@0.140.0.";
const governedContents = "governed fixture file contents";

function responseEnvelope(id: string, output: JsonObject[], status: string): JsonObject {
  return {
    id,
    object: "response",
    created_at: 1_788_278_400,
    status,
    error: null,
    incomplete_details: null,
    instructions: null,
    max_output_tokens: null,
    model: "openai/priced-model",
    output,
    parallel_tool_calls: true,
    previous_response_id: null,
    reasoning: { effort: null, summary: null },
    store: false,
    temperature: null,
    text: { format: { type: "text" } },
    tool_choice: "auto",
    tools: [],
    top_p: null,
    truncation: "disabled",
    usage:
      status === "completed"
        ? {
            input_tokens: 1,
            input_tokens_details: { cached_tokens: 0 },
            output_tokens: 1,
            output_tokens_details: { reasoning_tokens: 0 },
            total_tokens: 2,
          }
        : null,
  };
}

function stream(events: JsonObject[]): Response {
  const body = events
    .map((event) => `event: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`)
    .join("");
  return new Response(encoder.encode(`${body}data: [DONE]\n\n`), {
    headers: {
      "content-type": "text/event-stream",
      "cache-control": "no-cache",
    },
  });
}

function functionCall(
  responseId: string,
  itemId: string,
  callId: string,
  name: string,
  args: JsonObject,
  namespace?: string,
): Response {
  const argumentsJson = JSON.stringify(args);
  const pending = {
    id: itemId,
    type: "function_call",
    status: "in_progress",
    name,
    call_id: callId,
    arguments: "",
    ...(namespace ? { namespace } : {}),
  };
  const completed = { ...pending, status: "completed", arguments: argumentsJson };
  return stream([
    {
      type: "response.created",
      response: responseEnvelope(responseId, [], "in_progress"),
    },
    {
      type: "response.output_item.added",
      output_index: 0,
      item: pending,
    },
    {
      type: "response.function_call_arguments.delta",
      item_id: itemId,
      output_index: 0,
      delta: argumentsJson,
    },
    {
      type: "response.function_call_arguments.done",
      item_id: itemId,
      output_index: 0,
      name,
      arguments: argumentsJson,
    },
    {
      type: "response.output_item.done",
      output_index: 0,
      item: completed,
    },
    {
      type: "response.completed",
      response: responseEnvelope(responseId, [completed], "completed"),
    },
  ]);
}

function finalResponse(): Response {
  const responseId = "resp_steward_final";
  const itemId = "msg_steward_final";
  const pending = {
    id: itemId,
    type: "message",
    status: "in_progress",
    role: "assistant",
    content: [],
  };
  const part = { type: "output_text", text: declaredOutput, annotations: [] };
  const completed = { ...pending, status: "completed", content: [part] };
  return stream([
    {
      type: "response.created",
      response: responseEnvelope(responseId, [], "in_progress"),
    },
    {
      type: "response.output_item.added",
      output_index: 0,
      item: pending,
    },
    {
      type: "response.content_part.added",
      item_id: itemId,
      output_index: 0,
      content_index: 0,
      part: { ...part, text: "" },
    },
    {
      type: "response.output_text.delta",
      item_id: itemId,
      output_index: 0,
      content_index: 0,
      delta: declaredOutput,
    },
    {
      type: "response.output_text.done",
      item_id: itemId,
      output_index: 0,
      content_index: 0,
      text: declaredOutput,
    },
    {
      type: "response.content_part.done",
      item_id: itemId,
      output_index: 0,
      content_index: 0,
      part,
    },
    {
      type: "response.output_item.done",
      output_index: 0,
      item: completed,
    },
    {
      type: "response.completed",
      response: responseEnvelope(responseId, [completed], "completed"),
    },
  ]);
}

function tools(body: JsonObject): JsonObject[] {
  return Array.isArray(body.tools)
    ? body.tools.filter((tool): tool is JsonObject => typeof tool === "object" && tool !== null)
    : [];
}

function functionName(tool: JsonObject): string {
  return typeof tool.name === "string" ? tool.name : "";
}

function mcpFunctionTarget(tool: JsonObject): { name: string; namespace?: string } | null {
  if (tool.type === "function" && functionName(tool).endsWith("get_file_contents")) {
    return { name: functionName(tool) };
  }
  if (tool.type !== "namespace" || !functionName(tool).startsWith("mcp__")) return null;
  if (!Array.isArray(tool.tools)) return null;
  const nested = tool.tools.find(
    (candidate): candidate is JsonObject =>
      typeof candidate === "object" &&
      candidate !== null &&
      functionName(candidate).endsWith("get_file_contents"),
  );
  return nested ? { name: functionName(nested), namespace: functionName(tool) } : null;
}

function shellArguments(tool: JsonObject): JsonObject | null {
  const parameters = tool.parameters;
  if (typeof parameters !== "object" || parameters === null) return null;
  const properties = (parameters as JsonObject).properties;
  if (typeof properties !== "object" || properties === null) return null;
  const command = `mkdir -p out && printf '%s\\n' '${declaredOutput}' > out/result.txt`;
  const commandSchema = (properties as JsonObject).command;
  if (typeof commandSchema === "object" && commandSchema !== null) {
    return (commandSchema as JsonObject).type === "array"
      ? { command: [command] }
      : { command };
  }
  if ("cmd" in (properties as JsonObject)) return { cmd: command };
  return null;
}

Bun.serve({
  hostname: "0.0.0.0",
  port: 4000,
  async fetch(request) {
    const url = new URL(request.url);
    if (request.method === "GET" && url.pathname === "/health") {
      return new Response("ok\n");
    }
    if (request.method !== "POST" || url.pathname !== "/v1/responses") {
      return new Response("not found\n", { status: 404 });
    }
    const authorization = request.headers.get("authorization") ?? "";
    if (
      !authorization.startsWith("Bearer ") ||
      authorization === "Bearer openshell-token-grant-placeholder"
    ) {
      return new Response("runtime-derived authorization required\n", { status: 401 });
    }
    const body = (await request.json()) as JsonObject;
    const encodedInput = JSON.stringify(body.input ?? []);
    const advertisedTools = tools(body);

    if (!encodedInput.includes(governedContents)) {
      const mcpTool = advertisedTools.map(mcpFunctionTarget).find((tool) => tool !== null);
      if (!mcpTool) return new Response("Codex did not advertise the MCP tool\n", { status: 422 });
      return functionCall(
        "resp_steward_mcp",
        "fc_steward_mcp",
        "call_steward_mcp",
        mcpTool.name,
        { owner: "example-org", repo: "fixture-repository", path: "README.md" },
        mcpTool.namespace,
      );
    }

    if (!encodedInput.includes(declaredOutput)) {
      const shellTool = advertisedTools
        .map((tool) => ({ tool, args: shellArguments(tool) }))
        .find(({ tool, args }) => {
          const name = functionName(tool);
          return args !== null && (name === "shell" || name.includes("exec"));
        });
      if (!shellTool?.args) {
        return new Response("Codex did not advertise a writable shell tool\n", { status: 422 });
      }
      return functionCall(
        "resp_steward_shell",
        "fc_steward_shell",
        "call_steward_shell",
        functionName(shellTool.tool),
        shellTool.args,
      );
    }

    return finalResponse();
  },
});
