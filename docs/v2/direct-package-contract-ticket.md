# DP-C01: Freeze direct-package v2 contracts

Priority: P0 foundation

Status: implementation in progress on `feat/direct-package-contract`

## Goal

Define reviewable, provider-neutral schemas for direct invocation before the Identity,
Git source, Steward, runner, and package implementations proceed independently.

## Scope

- `steward.task/v2` invocation manifest with package repository, typed commit, path,
  Envelope digest, and optional diagnostics;
- direct TaskDefinition schema with prompt, runtime selection, outputs, optional
  skills, and optional complete `requires`;
- direct instruction-skill schema in which omitted kind means `instruction_only`;
- deterministic package-closure digest rules;
- verified source-provenance input shape;
- immutable Task source/effective-authority evidence shape;
- public v2 Task status carrying that immutable evidence; and
- successful-run stdout/stderr transcript filenames and limits.

The frozen `steward.m1/v1` schemas are inputs for compatibility tests only and are not
modified.

## Fixed semantics

- repository and commit remain separate fields;
- commit values are `git:sha1:<40-hex>` or the same-repository-only `git:trigger`;
- Envelope values are `steward:sha256:<64-hex>`;
- omitted or empty skills means no skills;
- instruction-only is the default and only initial skill kind;
- omitted `requires` expands to the selected Envelope's full approved authority
  values, including explicit null optional maxima;
- present `requires` is a complete narrower request;
- execution behavior comes only from the immutable deployment-owned `runtime.agentRef`
  binding; packages cannot declare unverified execution capabilities;
- omitted diagnostics means no caller-visible execution log; and
- `diagnostics.executionLog: full` requests successful stdout/stderr replay.

## Negative tests

- unknown contract versions and fields fail closed;
- malformed typed identities, mutable Git refs, and path escapes are rejected;
- `git:trigger` cannot identify a cross-repository package;
- duplicate paths, cycles, cross-source dependencies, and non-canonical encodings are
  rejected;
- unknown skill kinds cannot execute;
- partial present `requires` is invalid; and
- unknown diagnostic modes cannot enable logging.

## Exit criteria

- schemas, canonicalization, digest vectors, positive fixtures, and negative fixtures
  are reviewed;
- generated artifacts, if any, are produced through the repository's normal generator;
- compatibility tests prove frozen v1 is unchanged; and
- DP-I01, DP-G01, DP-R01, DP-A01, and DP-S01 can consume the contract without
  inventing local field meanings.

## Parallel boundary

This ticket lands first. The four independent implementation lanes may start from its
reviewed contract commit. Wire-compatible corrections are coordinated here rather than
made independently in consumers.

## Implementation record

- [x] additive Rust wire types and strict semantic validation;
- [x] authoritative JSON Schema and positive, negative, and v1 compatibility fixtures;
- [x] signed `source_provenance` exchange-JWT claim and authenticated v2 diagnostics
  response projection;
- [x] deterministic closure canonicalization and digest vector;
- [x] effective authority evidence preserves nullable Envelope maxima and rejects
  package-authored execution capabilities without a verifiable authority source;
- [x] reserved successful-run transcript paths and bounds;
- [ ] repository gate and reviewed contract commit;
- [ ] downstream lane synchronization against the exact commit.
