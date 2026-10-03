"use client";

import Link from "next/link";
import { useCallback, useMemo, useState } from "react";

import { listAdminMembers, type BrowserMemberView } from "@/api-client";
import { EmptyState, PageHeader, ResourceBoundary } from "@/components/workspace-ui";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";

type MemberFilter = "all" | "active" | "pending" | "disabled";

async function loadMembers(): Promise<{ data?: Array<BrowserMemberView>; response?: Response }> {
  const result = await listAdminMembers({ cache: "no-store", credentials: "same-origin" });
  if (!result.data || !result.response?.ok) return { response: result.response };
  return { data: result.data.members, response: result.response };
}

function displayName(member: BrowserMemberView): string {
  return member.displayName?.trim() || member.displayEmail;
}

function initials(member: BrowserMemberView): string {
  const source = member.displayName?.trim() || member.displayEmail.split("@")[0] || "?";
  const parts = source.split(/[\s._-]+/).filter(Boolean);
  return (parts.length > 1 ? `${parts[0]?.[0] ?? ""}${parts.at(-1)?.[0] ?? ""}` : source.slice(0, 2)).toUpperCase();
}

function statusLabel(state: string): string {
  if (state === "pending") return "Invited";
  return state.replaceAll("_", " ").replace(/^./, (character) => character.toUpperCase());
}

function relativeDate(value: string | null | undefined): string {
  if (!value) return "Never";
  const timestamp = Date.parse(value);
  if (!Number.isFinite(timestamp)) return "Not reported";
  const days = Math.floor((Date.now() - timestamp) / 86_400_000);
  if (days <= 0) return "Today";
  if (days === 1) return "Yesterday";
  if (days < 30) return `${days}d ago`;
  return new Intl.DateTimeFormat(undefined, { dateStyle: "medium" }).format(timestamp);
}

export function AdminMembersView() {
  const session = useSession();
  const load = useCallback(() => loadMembers(), []);
  const state = useApiResource(load);

  if (session.status !== "authenticated") {
    return <section aria-labelledby="page-title"><EmptyState title="Session unavailable"><p>The authoritative administrator session is not available.</p></EmptyState></section>;
  }
  return (
    <section aria-labelledby="page-title" className="space-y-5">
      <PageHeader
        description="People who can sign in to this organization, their access, and the member roles that decide which templates they can request."
        title="Members"
      />
      <ResourceBoundary state={state}>{(members) => <MembersTable members={members} />}</ResourceBoundary>
    </section>
  );
}

function MembersTable({ members }: Readonly<{ members: Array<BrowserMemberView> }>) {
  const [filter, setFilter] = useState<MemberFilter>("all");
  const [query, setQuery] = useState("");
  const counts = useMemo(() => ({
    all: members.length,
    active: members.filter((member) => member.state === "active").length,
    pending: members.filter((member) => member.state === "pending").length,
    disabled: members.filter((member) => member.state === "disabled" || member.state === "reconnect_required").length,
  }), [members]);
  const visible = useMemo(() => {
    const normalized = query.trim().toLocaleLowerCase();
    return members.filter((member) => {
      const stateMatches = filter === "all"
        || (filter === "disabled" ? member.state === "disabled" || member.state === "reconnect_required" : member.state === filter);
      const queryMatches = !normalized
        || member.displayEmail.toLocaleLowerCase().includes(normalized)
        || member.displayName?.toLocaleLowerCase().includes(normalized);
      return stateMatches && Boolean(queryMatches);
    });
  }, [filter, members, query]);
  const tabs: ReadonlyArray<readonly [MemberFilter, string]> = [
    ["all", "All"],
    ["active", "Active"],
    ["pending", "Invited"],
    ["disabled", "Disabled"],
  ];

  if (members.length === 0) {
    return <EmptyState title="No members"><p>No organization members have been recorded.</p></EmptyState>;
  }

  return (
    <div className="overflow-hidden rounded-panel border bg-panel">
      <div className="flex flex-wrap items-center justify-between gap-3 border-b px-5">
        <div aria-label="Member status" className="flex gap-5" role="tablist">
          {tabs.map(([key, label]) => (
            <button
              aria-selected={filter === key}
              className={`flex items-center gap-1.5 border-b-2 py-3.5 text-sm font-semibold ${filter === key ? "border-brand text-ink" : "border-transparent text-muted-ink"}`}
              key={key}
              onClick={() => setFilter(key)}
              role="tab"
              type="button"
            >
              {label}<span className="text-xs font-medium text-faint-ink">{counts[key]}</span>
            </button>
          ))}
        </div>
        <input
          aria-label="Search members"
          className="my-2 h-9 w-60 max-w-full rounded-control border bg-panel px-3 text-sm"
          onChange={(event) => setQuery(event.target.value)}
          placeholder="Search by name or email"
          type="search"
          value={query}
        />
      </div>
      <div className="overflow-x-auto">
        <div className="grid min-w-[860px] grid-cols-[minmax(240px,2fr)_96px_minmax(220px,2fr)_84px_120px_16px] gap-4 border-b bg-subtle px-5 py-2.5 text-xs font-medium text-muted-ink">
          <span>Member</span><span>Status</span><span>Access</span><span>Identities</span><span>Last sign-in</span><span />
        </div>
        {visible.map((member) => (
          <Link
            aria-label={`Open ${member.displayEmail}`}
            className="grid min-w-[860px] grid-cols-[minmax(240px,2fr)_96px_minmax(220px,2fr)_84px_120px_16px] items-center gap-4 border-t border-line-soft px-5 py-3 text-sm hover:bg-subtle"
            href={`/admin/members/${encodeURIComponent(member.userId)}`}
            key={member.userId}
          >
            <span className="flex min-w-0 items-center gap-3">
              <span className="flex size-8 shrink-0 items-center justify-center rounded-full bg-line-soft text-xs font-bold text-muted-ink">{initials(member)}</span>
              <span className="min-w-0"><span className="block truncate font-semibold">{displayName(member)}</span><span className="block truncate text-[13px] text-muted-ink">{member.displayEmail}</span></span>
            </span>
            <MemberStatus state={member.state} />
            <span className="flex flex-wrap items-center gap-1.5">
              {member.administrator ? <span className="inline-flex h-6.5 items-center rounded-full bg-brand-soft px-2.5 text-xs font-bold text-link">Admin</span> : null}
              {member.memberRoles.map((role) => <span className="inline-flex h-6.5 items-center rounded-full bg-line-soft px-2.5 font-mono text-sm" key={role}>{role}</span>)}
              {!member.administrator && member.memberRoles.length === 0 ? <span className="text-[13px] text-faint-ink">No roles</span> : null}
            </span>
            <span className="font-mono text-muted-ink">{member.identityCount || "–"}</span>
            <span className="text-[13px] text-muted-ink">{relativeDate(member.lastSignInAt)}</span>
            <span aria-hidden="true" className="text-faint-ink">›</span>
          </Link>
        ))}
        {visible.length === 0 ? <p className="px-5 py-7 text-center text-sm text-muted-ink">No members match this filter.</p> : null}
      </div>
    </div>
  );
}

export function MemberStatus({ state }: Readonly<{ state: string }>) {
  const className = state === "active"
    ? "bg-ok-soft text-ok"
    : state === "pending"
      ? "bg-warn-soft text-warn"
      : "bg-line-soft text-muted-ink";
  return <span><span className={`inline-flex h-6 items-center gap-1.5 rounded-full px-2.5 text-xs font-semibold ${className}`}><span className="size-1.5 rounded-full bg-current" />{statusLabel(state)}</span></span>;
}

export { displayName, initials, relativeDate };
