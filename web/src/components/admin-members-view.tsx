"use client";

import Link from "next/link";
import { useCallback, useMemo, useState } from "react";

import { createAdminMember, listAdminEnvelopeTemplates, listAdminMembers, type BrowserMemberInvitationResult, type BrowserMemberView } from "@/api-client";
import { TagSelect, type TagSelectOption } from "@/components/hs";
import { EmptyState, PageHeader, ResourceBoundary } from "@/components/workspace-ui";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";

type MemberFilter = "all" | "active" | "pending" | "disabled";

type MembersData = { members: Array<BrowserMemberView>; roleOptions: Array<string> };

async function loadMembers(refresh: number): Promise<{ data?: MembersData; response?: Response }> {
  void refresh;
  const [members, templates] = await Promise.all([
    listAdminMembers({ cache: "no-store", credentials: "same-origin" }),
    listAdminEnvelopeTemplates({ cache: "no-store", credentials: "same-origin" }),
  ]);
  if (!members.data || !members.response?.ok) return { response: members.response };
  if (!templates.data || !templates.response?.ok) return { response: templates.response };
  return { data: {
    members: members.data.members,
    roleOptions: [...new Set(templates.data.templates.flatMap((template) => template.memberRoles))].sort(),
  }, response: members.response };
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
  const [refresh, setRefresh] = useState(0);
  const [inviting, setInviting] = useState(false);
  const load = useCallback(() => loadMembers(refresh), [refresh]);
  const state = useApiResource(load);

  if (session.status !== "authenticated") {
    return <section aria-labelledby="page-title"><EmptyState title="Session unavailable"><p>The authoritative administrator session is not available.</p></EmptyState></section>;
  }
  return (
    <section aria-labelledby="page-title" className="space-y-5">
      <PageHeader
        actions={<button className="h-10 rounded-control bg-brand px-4 text-sm font-semibold text-on-brand" onClick={() => setInviting(true)} type="button">Invite people</button>}
        description="People who can sign in to this organization, their access, and the member roles that decide which templates they can request."
        title="Members"
      />
      <ResourceBoundary state={state}>{(data) => <>
        {inviting ? <InvitePanel csrf={session.value.csrf} onCancel={() => setInviting(false)} onInvited={() => { setInviting(false); setRefresh((value) => value + 1); }} roleOptions={data.roleOptions} /> : null}
        <MembersTable members={data.members} />
      </>}</ResourceBoundary>
    </section>
  );
}

function InvitePanel({ csrf, onCancel, onInvited, roleOptions }: Readonly<{ csrf: string; onCancel: () => void; onInvited: () => void; roleOptions: Array<string> }>) {
  const [emails, setEmails] = useState("");
  const [administrator, setAdministrator] = useState(false);
  const [memberRoles, setMemberRoles] = useState<Array<string>>([]);
  const [pending, setPending] = useState(false);
  const [results, setResults] = useState<Array<BrowserMemberInvitationResult>>([]);
  const [message, setMessage] = useState<string | null>(null);
  const options = roleOptions.map<TagSelectOption>((role) => ({ key: role, kind: "neutral", label: role }));

  async function submit() {
    const parsed = emails.split(",").map((email) => email.trim()).filter(Boolean);
    setPending(true);
    setMessage(null);
    const result = await createAdminMember({
      body: { administrator, emails: parsed, memberRoles },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": csrf },
    });
    setPending(false);
    if (!result.data || !result.response?.ok) {
      setMessage("Invitations could not be recorded.");
      return;
    }
    setResults(result.data.results);
    if (result.data.results.every((entry) => entry.status !== "invalid")) onInvited();
  }

  return <div className="space-y-4 rounded-panel border bg-panel p-5">
    <div><h2 className="text-lg font-semibold">Invite people</h2><p className="mt-1 text-sm text-muted-ink">Reserve members by their verified organization email.</p></div>
    <label className="block space-y-1.5 text-sm font-semibold">Email addresses<textarea aria-label="Email addresses" className="min-h-24 w-full rounded-control border bg-panel px-3 py-2 font-normal" onChange={(event) => setEmails(event.target.value)} placeholder="alice@example.com, bob@example.org" value={emails} /></label>
    <label className="block space-y-1.5 text-sm font-semibold">Access<select aria-label="Access" className="block h-10 w-full rounded-control border bg-panel px-3 font-normal" onChange={(event) => setAdministrator(event.target.value === "administrator")} value={administrator ? "administrator" : "member"}><option value="member">Member</option><option value="administrator">Administrator</option></select></label>
    <div className="space-y-1.5"><TagSelect addPlaceholder="Add a role…" emptyPlaceholder="Search roles…" label="Member roles" onChange={setMemberRoles} options={options} value={memberRoles} /><p className="text-xs text-muted-ink">Without a role, the member can sign in but can&apos;t request an envelope.</p></div>
    {results.length > 0 ? <ul className="space-y-1 text-sm">{results.map((entry) => <li key={entry.email}><span className="font-mono">{entry.email}</span> · {entry.status.replaceAll("_", " ")}</li>)}</ul> : null}
    {message ? <p className="text-sm text-err" role="alert">{message}</p> : null}
    <div className="flex flex-wrap items-center justify-between gap-3 border-t pt-4"><p className="text-xs text-muted-ink">Invitees become active after their first verified organization sign-in.</p><div className="flex gap-2"><button className="h-9 rounded-control border px-3 text-sm font-semibold" disabled={pending} onClick={onCancel} type="button">Cancel</button><button className="h-9 rounded-control bg-brand px-3 text-sm font-semibold text-on-brand disabled:opacity-50" disabled={pending || emails.trim().length === 0} onClick={() => void submit()} type="button">{pending ? "Sending…" : "Send invites"}</button></div></div>
  </div>;
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
