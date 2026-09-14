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

  test("distinguishes healthy, expiring, and expired connections", () => {
    expect(renderToStaticMarkup(<StatusBadge value="Connected" />)).toContain("status-badge-success");
    expect(renderToStaticMarkup(<StatusBadge value="Expiring soon" />)).toContain("status-badge-warning");
    expect(renderToStaticMarkup(<StatusBadge value="Credential expired" />)).toContain("status-badge-error");
  });
});
