import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { renderToStaticMarkup } from "react-dom/server";

import { TagSelect } from "./tag-select";

const source = readFileSync(new URL("./tag-select.tsx", import.meta.url), "utf8");

describe("tag select", () => {
  test("marks a selected option that is no longer available on its own chip", () => {
    const html = renderToStaticMarkup(<TagSelect
      label="Web access hosts"
      onChange={() => {}}
      options={[
        { key: "advisories.example.com:443", kind: "neutral", label: "advisories.example.com:443" },
        { disabled: true, key: "retired.example.com:443", kind: "neutral", label: "retired.example.com:443", note: "No longer available" },
      ]}
      value={["advisories.example.com:443", "retired.example.com:443"]}
    />);
    expect(html).toContain("No longer available");
    expect(html.split("No longer available")).toHaveLength(2);
    expect(html).toContain("ring-warn");
    expect(html).toContain('aria-label="Remove retired.example.com:443"');
  });

  test("shows no access-class pill for neutral options", () => {
    expect(source).toContain('option.kind === "neutral" ? null');
  });
});
