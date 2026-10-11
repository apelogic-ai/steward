import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import ts from "typescript";

const webSourceRoot = fileURLToPath(new URL("../", import.meta.url));

function sourceFiles(root: string): Array<string> {
  return readdirSync(root, { withFileTypes: true }).flatMap((entry) => {
    const path = join(root, entry.name);
    if (entry.isDirectory()) return entry.name === "api-client" ? [] : sourceFiles(path);
    return (entry.name.endsWith(".ts") || entry.name.endsWith(".tsx"))
      && !entry.name.endsWith(".d.ts")
      && !entry.name.includes(".test.")
      ? [path]
      : [];
  });
}

const presentationFields = new Set(["monthlyLimit", "singleRunLimit", "runtimeMinutesLimit", "ttl", "observedAmount"]);
const presentationHelpers = new Set(["formatDuration", "formatMoney"]);

function barePresentationValues(sourceText: string, path: string): Array<string> {
  const source = ts.createSourceFile(path, sourceText, ts.ScriptTarget.Latest, true, path.endsWith(".tsx") ? ts.ScriptKind.TSX : ts.ScriptKind.TS);
  const violations: Array<string> = [];
  function visit(node: ts.Node) {
    if (ts.isJsxExpression(node) && node.expression && !ts.isJsxAttribute(node.parent)) {
      const expression = node.expression;
      const fields = new Set<string>();
      function inspect(candidate: ts.Node) {
        if (ts.isConditionalExpression(candidate)) {
          inspect(candidate.whenTrue);
          inspect(candidate.whenFalse);
          return;
        }
        if (ts.isBinaryExpression(candidate) && candidate.operatorToken.kind === ts.SyntaxKind.AmpersandAmpersandToken) {
          inspect(candidate.right);
          return;
        }
        if (ts.isCallExpression(candidate)
          && ts.isIdentifier(candidate.expression)
          && presentationHelpers.has(candidate.expression.text)) return;
        if (candidate !== expression
          && (ts.isJsxElement(candidate) || ts.isJsxFragment(candidate) || ts.isJsxSelfClosingElement(candidate))) return;
        if (ts.isPropertyAccessExpression(candidate) && presentationFields.has(candidate.name.text)) fields.add(candidate.name.text);
        if (candidate === expression && ts.isIdentifier(candidate) && presentationFields.has(candidate.text)) fields.add(candidate.text);
        ts.forEachChild(candidate, inspect);
      }
      inspect(expression);
      if (fields.size) violations.push(`${[...fields].join(", ")}: ${node.getText(source)}`);
    }
    ts.forEachChild(node, visit);
  }
  visit(source);
  return violations;
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
    const banned = /\b(?:rust|authorit(?:y|ies|ative)|admission paths?|boundar(?:y|ies)|ledgers?|brokers?|brokered|server-owned|source of truth)\b/i;
    const violations = sourceFiles(webSourceRoot).flatMap((path) =>
      userVisibleText(path)
        .filter((value) => banned.test(value))
        .map((value) => `${path.replace(`${webSourceRoot}/`, "")}: ${value.trim()}`),
    );
    expect(violations).toEqual([]);
  });

  test("money and duration fields are not rendered without a formatter", () => {
    const fixture = "const View = ({ item }) => <p>{item.monthlyLimit}</p>;";
    expect(barePresentationValues(fixture, "fixture.tsx")).toEqual(["monthlyLimit: {item.monthlyLimit}"]);
    const violations = sourceFiles(webSourceRoot).flatMap((path) =>
      barePresentationValues(readFileSync(path, "utf8"), path).map((value) => `${path.replace(`${webSourceRoot}/`, "")}: ${value}`),
    );
    expect(violations).toEqual([]);
  });

  test("server failure reasons are not rendered as primary request messages", () => {
    const envelopeSource = readFileSync(join(webSourceRoot, "components", "envelope-views.tsx"), "utf8");
    const runNowSource = readFileSync(join(webSourceRoot, "components", "browser-run-now-view.tsx"), "utf8");
    const repositoriesSource = readFileSync(join(webSourceRoot, "data", "github-repositories.tsx"), "utf8");
    const connectionsSource = readFileSync(join(webSourceRoot, "components", "connections-view.tsx"), "utf8");
    const runViewsSource = readFileSync(join(webSourceRoot, "components", "run-views.tsx"), "utf8");
    for (const [name, source, disclosure] of [
      ["envelope request", envelopeSource, "DiagnosticDetails"],
      ["Run now", runNowSource, "DiagnosticDetails"],
      ["repository listing", repositoriesSource, "<details"],
      ["connection", connectionsSource, "<details"],
      ["run actions", runViewsSource, "DiagnosticDetails"],
    ]) {
      expect(source, `${name} diagnostics`).toContain(disclosure);
      expect(source, `${name} disclosure`).toContain("Details");
    }
    expect(envelopeSource).toContain("failureReason");
    expect(runNowSource).toContain("failureReason");
    expect(repositoriesSource).toContain("state.reason");
    expect(connectionsSource).toContain("startFailure.detail");
    expect(runViewsSource).toContain("run.rerunUnavailableReason");
  });
});
