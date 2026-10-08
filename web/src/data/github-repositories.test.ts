import { expect, test } from "bun:test";

import type { GithubRepositoriesResponse } from "@/api-client";

import {
  GITHUB_REPOSITORIES_KEY,
  githubRepositoriesSwrOptions,
  RepositoryListingError,
  repositoryResourceState,
} from "./github-repositories";

const listing: GithubRepositoriesResponse = {
  apiVersion: "steward.github-automation/v1",
  hasNextPage: false,
  login: "alice",
  page: 1,
  repositories: [],
};

test("the shared repository cache uses one key and never revalidates on its own", () => {
  expect(GITHUB_REPOSITORIES_KEY).toBe("github-repositories");
  expect(githubRepositoriesSwrOptions).toMatchObject({
    refreshInterval: 0,
    revalidateIfStale: false,
    revalidateOnFocus: false,
    revalidateOnReconnect: false,
    shouldRetryOnError: false,
  });
});

test("repository resource state keeps the loading, ready, and error shapes", () => {
  const failure = new RepositoryListingError("github_automation_unavailable", "bridge_contract");
  expect(repositoryResourceState({ data: undefined, error: undefined, isValidating: false })).toEqual({ status: "loading" });
  expect(repositoryResourceState({ data: undefined, error: undefined, isValidating: true })).toEqual({ status: "loading" });
  expect(repositoryResourceState({ data: listing, error: undefined, isValidating: false })).toEqual({ status: "ready", value: listing });
  expect(repositoryResourceState({ data: listing, error: undefined, isValidating: true })).toEqual({ status: "ready", value: listing });
  expect(repositoryResourceState({ data: undefined, error: failure, isValidating: false }))
    .toEqual({ status: "error", error: "github_automation_unavailable", reason: "bridge_contract" });
  expect(repositoryResourceState({ data: undefined, error: failure, isValidating: true })).toEqual({ status: "loading" });
  expect(repositoryResourceState({ data: listing, error: failure, isValidating: false }))
    .toEqual({ status: "error", error: "github_automation_unavailable", reason: "bridge_contract" });
  expect(repositoryResourceState({ data: undefined, error: new Error("network"), isValidating: false }))
    .toEqual({ status: "error", error: "repository_query_failed", reason: null });
});
