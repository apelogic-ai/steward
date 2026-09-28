import {
  allRuns,
  listAdminRequests,
  listRequests,
  myRuns,
  type AdminRequestStateFilter,
  type AdminRequestsResponse,
  type AllRunsResponse,
  type EnvelopeRequestStatus,
  type EnvelopeRequestsResponse,
  type MyRunsResponse,
} from "@/api-client";

const PAGE_SIZE = 100;

function invalidPaginationResponse(): Response {
  return new Response(null, { status: 502 });
}

export async function loadAllEnvelopeRequests(status?: EnvelopeRequestStatus) {
  const requests: EnvelopeRequestsResponse["requests"] = [];
  const seen = new Set<string>();
  let cursor: string | undefined;
  for (;;) {
    const page = await listRequests({ cache: "no-store", credentials: "same-origin", query: { cursor, limit: PAGE_SIZE, status } });
    if (!page.data || !page.response?.ok) return page;
    requests.push(...page.data.requests);
    const nextCursor = page.data.nextCursor ?? undefined;
    if (!nextCursor) return { data: { ...page.data, nextCursor: null, requests }, response: page.response };
    if (seen.has(nextCursor)) return { data: undefined, response: invalidPaginationResponse() };
    seen.add(nextCursor);
    cursor = nextCursor;
  }
}

export async function loadAllMyRuns() {
  const runs: MyRunsResponse["runs"] = [];
  const seen = new Set<string>();
  let cursor: string | undefined;
  for (;;) {
    const page = await myRuns({ cache: "no-store", credentials: "same-origin", query: { cursor, limit: PAGE_SIZE } });
    if (!page.data || !page.response?.ok) return page;
    runs.push(...page.data.runs);
    const nextCursor = page.data.nextCursor ?? undefined;
    if (!nextCursor) return { data: { ...page.data, nextCursor: null, runs }, response: page.response };
    if (seen.has(nextCursor)) return { data: undefined, response: invalidPaginationResponse() };
    seen.add(nextCursor);
    cursor = nextCursor;
  }
}

export async function loadAllRuns() {
  const runs: AllRunsResponse["runs"] = [];
  const seen = new Set<string>();
  let cursor: string | undefined;
  for (;;) {
    const page = await allRuns({ cache: "no-store", credentials: "same-origin", query: { cursor, limit: PAGE_SIZE } });
    if (!page.data || !page.response?.ok) return page;
    runs.push(...page.data.runs);
    const nextCursor = page.data.nextCursor ?? undefined;
    if (!nextCursor) return { data: { ...page.data, nextCursor: null, runs }, response: page.response };
    if (seen.has(nextCursor)) return { data: undefined, response: invalidPaginationResponse() };
    seen.add(nextCursor);
    cursor = nextCursor;
  }
}

export async function loadAllAdminRequests(state: AdminRequestStateFilter = "all") {
  const requests: AdminRequestsResponse["requests"] = [];
  const seen = new Set<string>();
  let cursor: string | undefined;
  for (;;) {
    const page = await listAdminRequests({ cache: "no-store", credentials: "same-origin", query: { cursor, limit: PAGE_SIZE, state } });
    if (!page.data || !page.response?.ok) return page;
    requests.push(...page.data.requests);
    const nextCursor = page.data.nextCursor ?? undefined;
    if (!nextCursor) return { data: { ...page.data, nextCursor: null, requests }, response: page.response };
    if (seen.has(nextCursor)) return { data: undefined, response: invalidPaginationResponse() };
    seen.add(nextCursor);
    cursor = nextCursor;
  }
}
