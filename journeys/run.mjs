import { readdirSync, readFileSync, statSync } from "node:fs";
import { spawnSync } from "node:child_process";

const config = "journeys/playwright.config.mjs";

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

  const runPrefix = requiredEnvironment("STEWARD_JOURNEY_RUN_PREFIX");
  if (!/^[a-z0-9][a-z0-9-]{0,39}$/.test(runPrefix)) {
    throw new Error("STEWARD_JOURNEY_RUN_PREFIX must be 1-40 lowercase letters, numbers, or hyphens, starting with a letter or number.");
  }

  return requiredEnvironment("STEWARD_JOURNEY_TAGS");
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
  process.stdout.write(`\n=== ${label}: validate step definitions ===\n`);
  return run("bunx", ["bddgen", "--config", config], {
    STEWARD_JOURNEY_EXECUTION: label,
    STEWARD_JOURNEY_TAG_EXPRESSION: tags,
  });
}

function execute(label, tags) {
  const environment = {
    STEWARD_JOURNEY_EXECUTION: label,
    STEWARD_JOURNEY_TAG_EXPRESSION: tags,
  };
  const generated = generate(label, tags);
  if (generated !== 0) return generated;
  process.stdout.write(`\n=== ${label}: browser scenarios ===\n`);
  return run("bunx", ["playwright", "test", "--config", config, "--pass-with-no-tests"], environment);
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

const knownFailingTagsExpression = knownFailingExpression();
const requiredTags = `(${selectedTags}) and not ${knownFailingTagsExpression}`;
const requiredStatus = execute("required", requiredTags);
if (requiredStatus !== 0) process.exit(requiredStatus);

const knownFailingTags = `(${selectedTags}) and ${knownFailingTagsExpression}`;
const knownFailingStatus = execute("known-failing", knownFailingTags);
if (knownFailingStatus === 0) {
  process.stdout.write("\nKnown-failing scenarios: none failed (the selected set may be empty).\n");
} else {
  process.stdout.write(`\nKnown-failing scenarios: failures reported above (nonfatal, status ${knownFailingStatus}).\n`);
}
