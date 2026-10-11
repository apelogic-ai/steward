import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import ts from "typescript";

const webSourceRoot = new URL("../", import.meta.url).pathname;

function sourceFiles(root: string): Array<string> {
  return readdirSync(root, { withFileTypes: true }).flatMap((entry) => {
    const path = join(root, entry.name);
    if (entry.isDirectory()) return entry.name === "api-client" ? [] : sourceFiles(path);
    return entry.name.endsWith(".tsx") && !entry.name.includes(".test.") ? [path] : [];
  });
}

function userVisibleText(path: string): Array<string> {
  const source = ts.createSourceFile(path, readFileSync(path, "utf8"), ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  const values: Array<string> = [];
  function visit(node: ts.Node) {
    const importOrExport = node.parent
      ? ts.isImportDeclaration(node.parent) || ts.isExportDeclaration(node.parent)
      : false;
    if ((ts.isStringLiteralLike(node)
      || ts.isJsxText(node)
      || ts.isTemplateHead(node)
      || ts.isTemplateMiddle(node)
      || ts.isTemplateTail(node)) && !importOrExport) values.push(node.text);
    ts.forEachChild(node, visit);
  }
  visit(source);
  return values;
}

describe("browser presentation rules", () => {
  test("user-visible copy contains no implementation jargon", () => {
    const banned = /\b(?:rust|authority|authoritative|admission path|boundary|ledger|broker|server-owned|source of truth)\b/i;
    const violations = sourceFiles(webSourceRoot).flatMap((path) =>
      userVisibleText(path)
        .filter((value) => banned.test(value))
        .map((value) => `${path.replace(`${webSourceRoot}/`, "")}: ${value.trim()}`),
    );
    expect(violations).toEqual([]);
  });

  test("money and duration presentation goes through shared helpers", () => {
    for (const file of [
      "admin-approvals-view.tsx",
      "admin-envelope-provision-view.tsx",
      "admin-template-view.tsx",
      "envelope-exceedance-summary.tsx",
      "envelope-views.tsx",
      "onboarding-view.tsx",
      "run-views.tsx",
    ]) {
      const source = readFileSync(join(webSourceRoot, "components", file), "utf8");
      expect(source, file).toContain("@/components/presentation-values");
    }
  });

  test("server failure reasons are not rendered as primary request messages", () => {
    const envelopeSource = readFileSync(join(webSourceRoot, "components", "envelope-views.tsx"), "utf8");
    const runNowSource = readFileSync(join(webSourceRoot, "components", "browser-run-now-view.tsx"), "utf8");
    const repositoriesSource = readFileSync(join(webSourceRoot, "data", "github-repositories.tsx"), "utf8");
    const connectionsSource = readFileSync(join(webSourceRoot, "components", "connections-view.tsx"), "utf8");
    const runViewsSource = readFileSync(join(webSourceRoot, "components", "run-views.tsx"), "utf8");
    expect(envelopeSource).not.toContain("setFailureReason(problem?.failureReason");
    expect(runNowSource).not.toContain("`${code ? `${code}: ` : \"\"}${reason}");
    expect(repositoriesSource).not.toContain("Repository listing failed: <code>");
    expect(connectionsSource).not.toContain("`GitHub authorization failed (${failure.error})");
    expect(connectionsSource).not.toContain("failure.detail ?");
    expect(runViewsSource).not.toContain("run.rerunUnavailableReason ??");
    expect(runViewsSource).not.toContain("cancelled (${cancelState})");
  });
});
