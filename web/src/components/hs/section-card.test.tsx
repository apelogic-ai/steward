import { expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";

import { SectionCard } from "./section-card";

test("section cards do not clip picker popovers", () => {
  const html = renderToStaticMarkup(<SectionCard footer="Footer" title="Models">Content</SectionCard>);

  expect(html).not.toContain("overflow-hidden rounded-card");
  expect(html).toContain("overflow-visible rounded-card");
  expect(html).toContain("rounded-b-card");
});
