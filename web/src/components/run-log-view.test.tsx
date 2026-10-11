import { expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";

import { ExecutionLogContent, shouldPollExecutionLog } from "./run-log-view";

test("a cancelled run stops polling and labels its retained live output as partial", () => {
  expect(shouldPollExecutionLog({ complete: true, truncated: false })).toBe(false);

  const html = renderToStaticMarkup(
    <ExecutionLogContent
      state={{
        status: "ready",
        complete: true,
        partial: true,
        text: "output captured before cancellation\n",
      }}
      stream="stdout"
    />,
  );

  expect(html).toContain("The run ended before the full log was saved.");
  expect(html).toContain("This is the output captured while it ran.");
  expect(html).not.toContain("polling for new output");
});
