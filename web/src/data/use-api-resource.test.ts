import { describe, expect, test } from "bun:test";

import { classifyResource } from "./use-api-resource";

describe("classifyResource", () => {
  test("keeps missing, forbidden, unavailable, and malformed responses distinct", () => {
    expect(classifyResource({ response: new Response(null, { status: 404 }) }).status).toBe("not-found");
    expect(classifyResource({ response: new Response(null, { status: 403 }) }).status).toBe("forbidden");
    expect(classifyResource({ response: new Response(null, { status: 503 }) }).status).toBe("unavailable");
    expect(classifyResource({ response: new Response(null, { status: 418 }) }).status).toBe("error");
  });
});
