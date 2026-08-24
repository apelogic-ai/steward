"use client";

import { useEffect, useState } from "react";

interface ApiResult<T> {
  data?: T;
  response?: Response;
}

export type ResourceState<T> =
  | { status: "loading" }
  | { status: "ready"; value: T }
  | { status: "not-found" }
  | { status: "forbidden" }
  | { status: "unavailable" }
  | { status: "error" };

export function classifyResource<T>(result: ApiResult<T>): ResourceState<T> {
  if (result.data !== undefined && result.response?.ok) return { status: "ready", value: result.data };
  if (result.response?.status === 404) return { status: "not-found" };
  if (result.response?.status === 401 || result.response?.status === 403) return { status: "forbidden" };
  if (!result.response || result.response.status === 502 || result.response.status === 503) {
    return { status: "unavailable" };
  }
  return { status: "error" };
}

export function useApiResource<T>(load: () => Promise<ApiResult<T>>): ResourceState<T> {
  const [settled, setSettled] = useState<{
    load?: () => Promise<ApiResult<T>>;
    state: ResourceState<T>;
  }>({ state: { status: "loading" } });

  useEffect(() => {
    let active = true;
    void load().then((result) => {
      if (active) setSettled({ load, state: classifyResource(result) });
    });
    return () => {
      active = false;
    };
  }, [load]);

  return settled.load === load ? settled.state : { status: "loading" };
}
