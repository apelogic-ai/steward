import { existsSync, readdirSync, readFileSync, rmSync, statSync } from "node:fs";
import { spawnSync } from "node:child_process";

import { expectedKnownFailures as reportHasExpectedKnownFailures } from "./known-failing-report.mjs";
import { browserInputsRequired } from "./run-environment.mjs";

const config = "journeys/playwright.config.mjs";
const knownFailingReport = "target/journeys/known-failing-report.json";
function knownFailingExpression() {
  const tags = new Set();
  for (const name of readdirSync("journeys").filter((candidate) => candidate.endsWith(".feature"))) {
    const feature = readFileSync(`journeys/${name}`, "utf8");
    for (const match of feature.matchAll(/@known-failing-[A-Za-z0-9._-]+/g)) tags.add(match[0]);
  }
  return tags.size ? `(${[...tags].sort().join(" or ")})` : "@__no_known_failing_scenarios__";
}

function requiredEnvironment(name) {
  const value = process.env[name]?.trim();
  if (!value) throw new Error(`${name} is required. See journeys/README.md.`);
  return value;
}

function validateEnvironment() {
  const selectedTags = requiredEnvironment("STEWARD_JOURNEY_TAGS");
  const baseUrl = requiredEnvironment("STEWARD_JOURNEY_BASE_URL");
  let parsedBaseUrl;
  try {
    parsedBaseUrl = new URL(baseUrl);
  } catch {
    throw new Error("STEWARD_JOURNEY_BASE_URL must be a valid HTTP or HTTPS URL.");
  }
  if (!(["http:", "https:"].includes(parsedBaseUrl.protocol))) {
    throw new Error("STEWARD_JOURNEY_BASE_URL must be a valid HTTP or HTTPS URL.");
  }

  if (browserInputsRequired(selectedTags)) {
    const storageState = requiredEnvironment("STEWARD_JOURNEY_STORAGE_STATE");
    try {
      if (!statSync(storageState).isFile()) throw new Error("not a file");
      const parsed = JSON.parse(readFileSync(storageState, "utf8"));
      if (!Array.isArray(parsed.cookies) || !Array.isArray(parsed.origins)) throw new Error("invalid shape");
    } catch (error) {
      throw new Error(`STEWARD_JOURNEY_STORAGE_STATE must name a readable Playwright storage-state JSON file: ${error.message}`);
    }

    const repository = requiredEnvironment("STEWARD_JOURNEY_REPOSITORY");
    if (!/^[^/\s]+\/[^/\s]+$/.test(repository)) {
      throw new Error("STEWARD_JOURNEY_REPOSITORY must use owner/repository form.");
    }

    requiredEnvironment("STEWARD_JOURNEY_MEMBER_ROLE");
  }

  const runPrefix = requiredEnvironment("STEWARD_JOURNEY_RUN_PREFIX");
  if (!/^[a-z0-9][a-z0-9-]{0,39}$/.test(runPrefix)) {
    throw new Error("STEWARD_JOURNEY_RUN_PREFIX must be 1-40 lowercase letters, numbers, or hyphens, starting with a letter or number.");
  }

  const ignoreHttpsErrors = process.env.STEWARD_JOURNEY_IGNORE_HTTPS_ERRORS?.trim();
  if (ignoreHttpsErrors && !["true", "false"].includes(ignoreHttpsErrors)) {
    throw new Error("STEWARD_JOURNEY_IGNORE_HTTPS_ERRORS must be true or false when set.");
  }

  return selectedTags;
}

function run(command, arguments_, environment) {
  const result = spawnSync(command, arguments_, {
    cwd: process.cwd(),
    env: { ...process.env, ...environment },
    stdio: "inherit",
  });
  return result.status ?? 1;
}

function generate(label, tags) {
  rmSync(`journeys/.generated/${label}`, { force: true, recursive: true });
  process.stdout.write(`\n=== ${label}: validate step definitions ===\n`);
  return run("bunx", ["bddgen", "--config", config], {
    STEWARD_JOURNEY_EXECUTION: label,
    STEWARD_JOURNEY_TAG_EXPRESSION: tags,
  });
}

function generatedScenarioCount(label) {
  const directory = `journeys/.generated/${label}`;
  if (!existsSync(directory)) return 0;
  return readdirSync(directory, { recursive: true })
    .filter((entry) => typeof entry === "string" && entry.endsWith(".spec.js"))
    .reduce((count, entry) => {
      const source = readFileSync(`${directory}/${entry}`, "utf8");
      return count + (source.match(/^\s*test\('/gm)?.length ?? 0);
    }, 0);
}

function execute(label, tags) {
  const environment = {
    STEWARD_JOURNEY_EXECUTION: label,
    STEWARD_JOURNEY_TAG_EXPRESSION: tags,
  };
  const generated = generate(label, tags);
  if (generated !== 0) return generated;
  process.stdout.write(`\n=== ${label}: browser scenarios ===\n`);
  return run("bunx", ["playwright", "test", "--config", config], environment);
}

function expectedKnownFailures(count) {
  if (!existsSync(knownFailingReport)) return false;
  let report;
  try {
    report = JSON.parse(readFileSync(knownFailingReport, "utf8"));
  } catch {
    return false;
  }
  return reportHasExpectedKnownFailures(report, count);
}

let selectedTags;
try {
  selectedTags = validateEnvironment();
} catch (error) {
  process.stderr.write(`journeys: ${error.message}\n`);
  process.exit(2);
}

if (generate("all-selected", selectedTags) !== 0) {
  process.stderr.write("journeys: a selected sentence has no step definition; no browser scenario was run.\n");
  process.exit(1);
}
if (generatedScenarioCount("all-selected") === 0) {
  process.stderr.write(`journeys: tag expression ${JSON.stringify(selectedTags)} selected no scenarios.\n`);
  process.exit(1);
}

const knownFailingTagsExpression = knownFailingExpression();
const requiredTags = `(${selectedTags}) and not ${knownFailingTagsExpression}`;
const requiredStatus = execute("required", requiredTags);
if (requiredStatus !== 0) process.exit(requiredStatus);

const knownFailingTags = `(${selectedTags}) and ${knownFailingTagsExpression}`;
const knownFailingGenerated = generate("known-failing", knownFailingTags);
if (knownFailingGenerated !== 0) process.exit(knownFailingGenerated);
const knownFailingCount = generatedScenarioCount("known-failing");
process.stdout.write(`\nKnown-failing scenarios selected: ${knownFailingCount}.\n`);
if (knownFailingCount > 0) {
  process.stdout.write("\n=== known-failing: browser scenarios ===\n");
  rmSync(knownFailingReport, { force: true });
  const knownFailingStatus = run("bunx", ["playwright", "test", "--config", config], {
    STEWARD_JOURNEY_EXECUTION: "known-failing",
    STEWARD_JOURNEY_TAG_EXPRESSION: knownFailingTags,
  });
  if (!expectedKnownFailures(knownFailingCount)) {
    process.stderr.write("journeys: a known-failing scenario unexpectedly passed or could not complete.\n");
    process.exit(knownFailingStatus || 1);
  }
  process.stdout.write(`\nKnown-failing scenarios: ${knownFailingCount} failed as expected.\n`);
}
