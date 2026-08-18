"use strict";

const assert = require("node:assert/strict");
const path = require("node:path");

require(path.join(__dirname, "preview-readiness.js"));

const { PreviewReadinessState, actions } = globalThis.StewardPreviewReadiness;

function deferred() {
  let resolve;
  const promise = new Promise((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

async function delayedOutcome(state, generation, pending) {
  const outcome = await pending.promise;
  if (outcome === "success") {
    return state.acceptSuccess(generation, true);
  }
  return state.acceptRetryableFailure(generation);
}

async function main() {
  const state = new PreviewReadinessState();
  const staleFailure = deferred();
  const currentSuccess = deferred();

  const oldGeneration = state.begin();
  const oldRequest = delayedOutcome(state, oldGeneration, staleFailure);
  const currentGeneration = state.begin();
  const currentRequest = delayedOutcome(state, currentGeneration, currentSuccess);

  currentSuccess.resolve("success");
  assert.equal(await currentRequest, actions.CHECKING);
  assert.equal(state.acceptSuccess(currentGeneration, true), actions.READY);

  staleFailure.resolve("failure");
  assert.equal(await oldRequest, actions.IGNORE);
  assert.equal(state.stable, true, "a delayed stale failure must not overwrite stable readiness");

  assert.equal(
    state.acceptRetryableFailure(currentGeneration),
    actions.HOLD_READY,
    "one current transient failure must preserve the stable display"
  );
  assert.equal(state.acceptSuccess(currentGeneration, true), actions.READY);
  assert.equal(state.acceptRetryableFailure(currentGeneration), actions.HOLD_READY);
  assert.equal(
    state.acceptRetryableFailure(currentGeneration),
    actions.CHECKING,
    "two current failures must make the action unavailable"
  );
  assert.equal(state.acceptTerminal(currentGeneration), actions.TERMINAL);
}

main().catch((error) => {
  process.stderr.write(`${error.stack}\n`);
  process.exitCode = 1;
});
