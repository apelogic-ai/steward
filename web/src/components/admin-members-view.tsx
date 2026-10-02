"use client";

import { useCallback, useState, type FormEvent } from "react";

import {
  associateAdminFederatedSubject,
  changeAdminMemberRole,
  createAdminMember,
  disableAdminFederatedSubject,
  listAdminFederatedSubjects,
  listAdminMembers,
  type BrowserFederatedSubjectView,
  type BrowserMemberAssignmentAction,
  type BrowserMemberView,
} from "@/api-client";
import { EmptyState, PageHeader, ResourceBoundary } from "@/components/workspace-ui";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";

type MembersData = {
  members: Array<BrowserMemberView>;
  subjects: Array<BrowserFederatedSubjectView>;
};

async function loadMembers(refresh: number): Promise<{ data?: MembersData; response?: Response }> {
  void refresh;
  const [members, subjects] = await Promise.all([
    listAdminMembers({ cache: "no-store", credentials: "same-origin" }),
    listAdminFederatedSubjects({ cache: "no-store", credentials: "same-origin" }),
  ]);
  if (!members.data || !members.response?.ok) return { response: members.response };
  if (!subjects.data || !subjects.response?.ok) return { response: subjects.response };
  return {
    data: { members: members.data.members, subjects: subjects.data.federatedSubjects },
    response: members.response,
  };
}

function mutationMessage(status: number | undefined): string {
  if (status === 409) return "The requested change conflicts with current authority. Reload and try again.";
  if (status === 401 || status === 403) return "Your administrator session no longer permits this action.";
  if (status === 422) return "The requested member change is not valid.";
  if (status === 503) return "The member service is unavailable. No change was confirmed.";
  return "The change could not be completed.";
}

export function AdminMembersView() {
  const session = useSession();
  const [reload, setReload] = useState(0);
  const load = useCallback(() => loadMembers(reload), [reload]);
  const state = useApiResource(load);

  if (session.status !== "authenticated") {
    return <section aria-labelledby="page-title"><EmptyState title="Session unavailable"><p>The authoritative administrator session is not available.</p></EmptyState></section>;
  }
  return (
    <section aria-labelledby="page-title" className="space-y-6">
      <PageHeader description="Invite verified organization email addresses, assign local roles, and review federated identity links." title="Members" />
      <ResourceBoundary state={state}>{(data) => <MembersWorkspace csrf={session.value.csrf} data={data} onChanged={() => setReload((value) => value + 1)} />}</ResourceBoundary>
    </section>
  );
}

function MembersWorkspace({ csrf, data, onChanged }: Readonly<{ csrf: string; data: MembersData; onChanged: () => void }>) {
  const [email, setEmail] = useState("");
  const [inviteStatus, setInviteStatus] = useState<"idle" | "saving" | "done" | "error">("idle");
  const [message, setMessage] = useState<string | null>(null);

  async function invite(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setInviteStatus("saving");
    setMessage(null);
    const result = await createAdminMember({
      body: { email },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": csrf },
    });
    if (result.data && result.response?.ok) {
      setEmail("");
      setInviteStatus("done");
      setMessage(`Reserved ${result.data.member.displayEmail} for verified sign-in.`);
      onChanged();
      return;
    }
    setInviteStatus("error");
    setMessage(mutationMessage(result.response?.status));
  }

  return (
    <div className="space-y-6">
      <form className="flex flex-wrap items-end gap-3 rounded-panel border bg-panel p-5" onSubmit={invite}>
        <label className="grid min-w-64 flex-1 gap-2 text-sm font-semibold">Member email
          <input autoComplete="email" className="min-h-11 rounded-md border bg-panel px-3 font-normal" disabled={inviteStatus === "saving"} onChange={(event) => setEmail(event.target.value)} placeholder="alice@example.com" required type="email" value={email} />
        </label>
        <button className="min-h-11 rounded-md bg-brand px-4 py-2 text-sm font-semibold text-on-brand disabled:opacity-50" disabled={inviteStatus === "saving"} type="submit">{inviteStatus === "saving" ? "Adding…" : "Add member"}</button>
        <p className="basis-full text-sm text-muted-ink">A pending member becomes active only after an exact verified organization sign-in. Adding a member does not grant a role.</p>
        {message ? <p className={inviteStatus === "error" ? "basis-full text-sm text-err" : "basis-full text-sm text-success"} role="status">{message}</p> : null}
      </form>

      {data.members.length === 0 ? <EmptyState title="No members"><p>Add a verified organization email address to begin onboarding.</p></EmptyState> : <div className="grid gap-4">{data.members.map((member) => <MemberCard csrf={csrf} key={member.userId} member={member} onChanged={onChanged} />)}</div>}

      <IdentityLinks csrf={csrf} members={data.members} onChanged={onChanged} subjects={data.subjects} />
    </div>
  );
}

function MemberCard({ csrf, member, onChanged }: Readonly<{ csrf: string; member: BrowserMemberView; onChanged: () => void }>) {
  const [role, setRole] = useState("");
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);

  async function change(kind: "administrator" | "member_role", action: BrowserMemberAssignmentAction, memberRole?: string) {
    if (action === "revoke" && !window.confirm(`Revoke ${kind === "administrator" ? "administrator access" : `the ${memberRole} role`} from ${member.displayEmail}?`)) return;
    setBusy(true);
    setMessage(null);
    const result = await changeAdminMemberRole({
      body: { action, kind, ...(memberRole ? { memberRole } : {}) },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": csrf },
      path: { user_id: member.userId },
    });
    setBusy(false);
    if (result.data && result.response?.ok) {
      setRole("");
      onChanged();
      return;
    }
    setMessage(mutationMessage(result.response?.status));
  }

  return (
    <article className="rounded-panel border bg-panel p-5" aria-label={member.displayEmail}>
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h2 className="font-semibold">{member.displayEmail}</h2>
          <code className="mt-1 block text-xs text-muted-ink">{member.userId}</code>
        </div>
        <span className="rounded-full border px-2.5 py-1 text-xs font-semibold capitalize">{member.state.replaceAll("_", " ")}</span>
      </div>
      <div className="mt-5 grid gap-4 lg:grid-cols-2">
        <section aria-label="Administrator access" className="rounded-md border bg-canvas p-4">
          <h3 className="text-sm font-semibold">Administrator</h3>
          <p className="mt-1 text-sm text-muted-ink">{member.administrator ? "Can administer the organization." : "No administrator access."}</p>
          <button className="mt-3 min-h-10 rounded-control border px-3 text-sm font-semibold disabled:opacity-50" disabled={busy} onClick={() => void change("administrator", member.administrator ? "revoke" : "grant")} type="button">{member.administrator ? "Revoke administrator" : "Grant administrator"}</button>
        </section>
        <section aria-label="Member roles" className="rounded-md border bg-canvas p-4">
          <h3 className="text-sm font-semibold">Member roles</h3>
          <div className="mt-2 flex flex-wrap gap-2">{member.memberRoles.length === 0 ? <span className="text-sm text-muted-ink">No roles assigned.</span> : member.memberRoles.map((assignedRole) => <span className="inline-flex items-center gap-1 rounded-full border bg-panel py-1 ps-2.5 pe-1 text-xs" key={assignedRole}>{assignedRole}<button aria-label={`Revoke ${assignedRole}`} className="rounded-full px-1 text-muted-ink hover:bg-err-soft hover:text-err disabled:opacity-50" disabled={busy} onClick={() => void change("member_role", "revoke", assignedRole)} type="button">×</button></span>)}</div>
          <form className="mt-3 flex gap-2" onSubmit={(event) => { event.preventDefault(); void change("member_role", "grant", role.trim()); }}>
            <input className="min-h-10 min-w-0 flex-1 rounded-control border bg-panel px-3 text-sm" disabled={busy} onChange={(event) => setRole(event.target.value)} placeholder="engineering:member" required value={role} />
            <button className="min-h-10 rounded-control border px-3 text-sm font-semibold disabled:opacity-50" disabled={busy} type="submit">Grant</button>
          </form>
        </section>
      </div>
      {message ? <p className="mt-3 text-sm text-err" role="alert">{message}</p> : null}
    </article>
  );
}

function IdentityLinks({ csrf, members, onChanged, subjects }: Readonly<{ csrf: string; members: Array<BrowserMemberView>; onChanged: () => void; subjects: Array<BrowserFederatedSubjectView> }>) {
  const [targets, setTargets] = useState<Record<string, string>>({});
  const [message, setMessage] = useState<string | null>(null);
  const [busySubjectId, setBusySubjectId] = useState<string | null>(null);

  async function associate(subject: BrowserFederatedSubjectView) {
    const canonicalUserId = targets[subject.subjectId];
    if (!canonicalUserId) return;
    setBusySubjectId(subject.subjectId);
    setMessage(null);
    const result = await associateAdminFederatedSubject({
      body: { canonicalUserId, expectedRevision: subject.revision },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": csrf },
      path: { subject_id: subject.subjectId },
    });
    setBusySubjectId(null);
    if (result.data && result.response?.ok) {
      onChanged();
      return;
    }
    setMessage(mutationMessage(result.response?.status));
  }

  async function disable(subject: BrowserFederatedSubjectView) {
    if (!window.confirm(`Disable this federated identity for ${subject.displayName ?? subject.subject}?`)) return;
    setBusySubjectId(subject.subjectId);
    setMessage(null);
    const result = await disableAdminFederatedSubject({
      body: { expectedRevision: subject.revision, reason: "disabled by administrator" },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": csrf },
      path: { subject_id: subject.subjectId },
    });
    setBusySubjectId(null);
    if (result.data && result.response?.ok) {
      onChanged();
      return;
    }
    setMessage(mutationMessage(result.response?.status));
  }

  return (
    <section aria-labelledby="identity-links-title" className="space-y-4 rounded-panel border bg-panel p-5">
      <div>
        <h2 className="font-semibold" id="identity-links-title">Federated identity links</h2>
        <p className="mt-1 text-sm text-muted-ink">Associate an observed identity with a canonical member only after verifying its owner.</p>
      </div>
      {subjects.length === 0 ? <p className="text-sm text-muted-ink">No federated identities have been observed.</p> : <div className="space-y-3">{subjects.map((subject) => {
        const linkedMember = members.find((member) => member.userId === subject.canonicalUserId);
        const busy = busySubjectId === subject.subjectId;
        return <div className="flex flex-wrap items-center justify-between gap-3 rounded-md border bg-canvas p-4" key={subject.subjectId}>
          <div className="min-w-0">
            <p className="truncate text-sm font-semibold">{subject.displayName ?? subject.subject}</p>
            <p className="mt-1 truncate font-mono text-xs text-muted-ink">{subject.issuer} · {subject.subject}</p>
            <p className="mt-1 text-xs text-muted-ink">{subject.state}{linkedMember ? ` · linked to ${linkedMember.displayEmail}` : " · unassociated"}</p>
          </div>
          {linkedMember ? <button className="min-h-10 rounded-control border px-3 text-sm font-semibold text-err disabled:opacity-50" disabled={busy || subject.state === "disabled"} onClick={() => void disable(subject)} type="button">{subject.state === "disabled" ? "Disabled" : "Disable identity"}</button> : <div className="flex flex-wrap gap-2"><select className="min-h-10 max-w-56 rounded-control border bg-panel px-3 text-sm" onChange={(event) => setTargets((current) => ({ ...current, [subject.subjectId]: event.target.value }))} value={targets[subject.subjectId] ?? ""}><option value="">Choose member</option>{members.map((member) => <option key={member.userId} value={member.userId}>{member.displayEmail}</option>)}</select><button className="min-h-10 rounded-control border px-3 text-sm font-semibold disabled:opacity-50" disabled={busy || !targets[subject.subjectId]} onClick={() => void associate(subject)} type="button">Associate</button></div>}
        </div>;
      })}</div>}
      {message ? <p className="text-sm text-err" role="alert">{message}</p> : null}
    </section>
  );
}
