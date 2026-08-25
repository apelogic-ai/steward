"use client";

import { createContext, useContext, useEffect, useState, type ReactNode } from "react";

import { session as requestSession, type SessionResponse } from "@/api-client";
import { client } from "@/api-client/client.gen";
import { authStartPath } from "@/session/auth-redirect";

export type SessionState =
  | { status: "loading" }
  | { status: "authenticated"; value: SessionResponse }
  | { status: "unauthorized" }
  | { status: "unavailable" }
  | { status: "error" };

interface SessionResult {
  data?: SessionResponse;
  response?: Response;
}

export function classifySessionResult(result: SessionResult): SessionState {
  if (result.data && result.response?.status === 200) {
    return { status: "authenticated", value: result.data };
  }
  if (result.response?.status === 401) {
    return { status: "unauthorized" };
  }
  if (!result.response || result.response.status === 503) {
    return { status: "unavailable" };
  }
  return { status: "error" };
}

const SessionContext = createContext<SessionState>({ status: "loading" });

export function SessionProvider({ children }: Readonly<{ children: ReactNode }>) {
  const [state, setState] = useState<SessionState>({ status: "loading" });

  useEffect(() => {
    let active = true;
    const interceptor = client.interceptors.response.use((response) => {
      if (response.status === 401 && window.location.pathname !== "/admin/sign-in") {
        window.location.replace(authStartPath(window.location.pathname));
      }
      return response;
    });
    void requestSession({
      cache: "no-store",
      credentials: "same-origin",
    }).then((result) => {
      if (active) {
        setState(classifySessionResult(result));
      }
    });
    return () => {
      active = false;
      client.interceptors.response.eject(interceptor);
    };
  }, []);

  return <SessionContext value={state}>{children}</SessionContext>;
}

export function useSession(): SessionState {
  return useContext(SessionContext);
}
