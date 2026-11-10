// TEMPORARY #77 diagnostic: how does a Bun standalone executable see its environment?
const fs = require("node:fs");
let procEnviron;
try {
  procEnviron = fs.readFileSync("/proc/self/environ").toString("utf8").split("\0").filter(Boolean).map((e) => e.split("=")[0]);
} catch (error) {
  procEnviron = `error ${error.code ?? error.name}`;
}
console.log(JSON.stringify({
  processEnvKeys: Object.keys(process.env).sort(),
  bunEnvKeys: typeof Bun !== "undefined" ? Object.keys(Bun.env).sort() : "no Bun",
  hasProbeVar: process.env.PROBE_VAR === "1",
  procSelfEnviron: procEnviron,
  execPath: process.execPath,
  argv: process.argv,
}));
