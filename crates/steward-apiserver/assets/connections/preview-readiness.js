"use strict";

(function installPreviewReadinessState(root) {
  const IGNORE = "ignore";
  const CHECKING = "checking";
  const READY = "ready";
  const HOLD_READY = "hold_ready";
  const TERMINAL = "terminal";

  class PreviewReadinessState {
    constructor() {
      this.generation = 0;
      this.consecutiveSuccesses = 0;
      this.consecutiveFailures = 0;
      this.stable = false;
    }

    begin() {
      this.generation += 1;
      this.consecutiveSuccesses = 0;
      this.consecutiveFailures = 0;
      this.stable = false;
      return this.generation;
    }

    cancel() {
      this.begin();
    }

    owns(generation) {
      return generation === this.generation;
    }

    acceptSuccess(generation, runtimeRunning) {
      if (!this.owns(generation)) {
        return IGNORE;
      }
      this.consecutiveFailures = 0;
      if (!runtimeRunning) {
        this.consecutiveSuccesses = 0;
        this.stable = false;
        return CHECKING;
      }
      this.consecutiveSuccesses += 1;
      if (this.stable || this.consecutiveSuccesses >= 2) {
        this.stable = true;
        return READY;
      }
      return CHECKING;
    }

    acceptRetryableFailure(generation) {
      if (!this.owns(generation)) {
        return IGNORE;
      }
      this.consecutiveSuccesses = 0;
      this.consecutiveFailures += 1;
      if (this.stable && this.consecutiveFailures === 1) {
        return HOLD_READY;
      }
      this.stable = false;
      return CHECKING;
    }

    acceptTerminal(generation) {
      if (!this.owns(generation)) {
        return IGNORE;
      }
      this.consecutiveSuccesses = 0;
      this.consecutiveFailures = 0;
      this.stable = false;
      return TERMINAL;
    }
  }

  root.StewardPreviewReadiness = Object.freeze({
    PreviewReadinessState,
    actions: Object.freeze({ IGNORE, CHECKING, READY, HOLD_READY, TERMINAL }),
  });
})(globalThis);
