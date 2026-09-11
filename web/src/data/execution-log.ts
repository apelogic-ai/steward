export type ExecutionLogStream = "stderr" | "stdout";

export function isExecutionLogStream(value: string): value is ExecutionLogStream {
  return value === "stdout" || value === "stderr";
}
