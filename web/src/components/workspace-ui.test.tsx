import { describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";

import { StatusBadge } from "./workspace-ui";

describe("status badges", () => {
  test("keeps provisioned status at its intrinsic pill dimensions", () => {
    const html = renderToStaticMarkup(<StatusBadge value="Provisioned" />);

    expect(html).toContain("self-start");
    expect(html).toContain("w-fit");
    expect(html).toContain("shrink-0");
    expect(html).toContain("rounded-full border px-2.5 py-1");
    expect(html).toContain("status-badge-success");
  });
});
