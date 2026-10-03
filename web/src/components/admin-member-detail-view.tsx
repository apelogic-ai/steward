"use client";

import Link from "next/link";
import { useCallback, useMemo, useState } from "react";

import {
  associateAdminFederatedSubject,
  changeAdminMemberRole,
  getAdminMember,
  listAdminEnvelopeTemplates,
  listAdminFederatedSubjects,
  unlinkAdminMemberIdentity,
  type BrowserFederatedSubjectView,
  type BrowserMemberAssignmentAction,
  type BrowserMemberDetailView,
} from "@/api-client";
import { ConfirmationDialog, TagSelect, type TagSelectOption } from "@/components/hs";
import { displayName, initials, MemberStatus, relativeDate } from "@/components/admin-members-view";
import { EmptyState, ResourceBoundary } from "@/components/workspace-ui";
import { useApiResource } from "@/data/use-api-resource";
import { useSession } from "@/session/session-context";

type MemberDetailData = {
  member: BrowserMemberDetailView;
  roleOptions: Array<string>;
  unassociatedIdentities: Array<BrowserFederatedSubjectView>;
};

type Confirmation =
  | { kind: "administrator"; action: BrowserMemberAssignmentAction }
  | { kind: "unlink"; subjectId: string; revision: number; label: string }
  | { kind: "link"; subject: BrowserFederatedSubjectView }
  | null;

async function loadMember(userId: string, refresh: number): Promise<{ data?: MemberDetailData; response?: Response }> {
  void refresh;
  const [detail, templates, subjects] = await Promise.all([
    getAdminMember({ cache: "no-store", credentials: "same-origin", path: { user_id: userId } }),
    listAdminEnvelopeTemplates({ cache: "no-store", credentials: "same-origin" }),
    listAdminFederatedSubjects({ cache: "no-store", credentials: "same-origin" }),
  ]);
  if (!detail.data || !detail.response?.ok) return { response: detail.response };
  if (!templates.data || !templates.response?.ok) return { response: templates.response };
  if (!subjects.data || !subjects.response?.ok) return { response: subjects.response };
  return {
    data: {
      member: detail.data.member,
      roleOptions: [...new Set(templates.data.templates.flatMap((template) => template.memberRoles))].sort(),
      unassociatedIdentities: subjects.data.federatedSubjects.filter((subject) => subject.state === "observed" && !subject.canonicalUserId),
    },
    response: detail.response,
  };
}

function mutationMessage(status: number | undefined): string {
  if (status === 409) return "This authority changed before the operation completed. Reload and try again.";
  if (status === 401 || status === 403) return "Your administrator session no longer permits this action.";
  if (status === 404) return "This member or identity is no longer available.";
  if (status === 422) return "The requested member change is not valid.";
  if (status === 503) return "The member service is unavailable. No change was confirmed.";
  return "The change could not be completed.";
}

export function AdminMemberDetailView({ userId }: Readonly<{ userId: string }>) {
  const session = useSession();
  const [refresh, setRefresh] = useState(0);
  const load = useCallback(() => loadMember(userId, refresh), [refresh, userId]);
  const state = useApiResource(load);

  if (session.status !== "authenticated") {
    return <section aria-labelledby="page-title"><EmptyState title="Session unavailable"><p>The authoritative administrator session is not available.</p></EmptyState></section>;
  }
  return (
    <section aria-labelledby="page-title" className="max-w-5xl space-y-5">
      <ResourceBoundary state={state}>{(data) => (
        <MemberDetail
          csrf={session.value.csrf}
          currentUserId={session.value.principal.userId}
          data={data}
          onChanged={() => setRefresh((value) => value + 1)}
        />
      )}</ResourceBoundary>
    </section>
  );
}

function MemberDetail({ csrf, currentUserId, data, onChanged }: Readonly<{
  csrf: string;
  currentUserId: string;
  data: MemberDetailData;
  onChanged: () => void;
}>) {
  const { member } = data;
  const self = currentUserId === member.userId;
  const [busy, setBusy] = useState(false);
  const [confirmation, setConfirmation] = useState<Confirmation>(null);
  const [identityId, setIdentityId] = useState("");
  const [showIdentityPicker, setShowIdentityPicker] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const roleOptions = useMemo<Array<TagSelectOption>>(() => {
    const available = data.roleOptions.map((role) => ({ key: role, kind: "neutral" as const, label: role }));
    const stale = member.memberRoles
      .filter((role) => !data.roleOptions.includes(role))
      .map((role) => ({ disabled: true, key: role, kind: "neutral" as const, label: role, note: "No template" }));
    return [...available, ...stale];
  }, [data.roleOptions, member.memberRoles]);

  async function changeRole(action: BrowserMemberAssignmentAction, memberRole: string) {
    setBusy(true);
    setMessage(null);
    const result = await changeAdminMemberRole({
      body: { action, kind: "member_role", memberRole },
      cache: "no-store",
      credentials: "same-origin",
      headers: { "X-Steward-CSRF": csrf },
      path: { user_id: member.userId },
    });
    setBusy(false);
    if (result.data && result.response?.ok) {
      onChanged();
      return;
    }
    setMessage(mutationMessage(result.response?.status));
  }

  async function changeRoles(next: Array<string>) {
    if (busy) return;
    const added = next.find((role) => !member.memberRoles.includes(role));
    const removed = member.memberRoles.find((role) => !next.includes(role));
    if (added) await changeRole("grant", added);
    else if (removed) await changeRole("revoke", removed);
  }

  async function confirmChange() {
    if (!confirmation) return;
    setBusy(true);
    setMessage(null);
    let response: Response | undefined;
    if (confirmation.kind === "administrator") {
      const result = await changeAdminMemberRole({
        body: { action: confirmation.action, kind: "administrator" },
        cache: "no-store",
        credentials: "same-origin",
        headers: { "X-Steward-CSRF": csrf },
        path: { user_id: member.userId },
      });
      response = result.response;
      if (result.data && response?.ok) {
        setConfirmation(null);
        setBusy(false);
        onChanged();
        return;
      }
    } else if (confirmation.kind === "unlink") {
      const result = await unlinkAdminMemberIdentity({
        body: { expectedRevision: confirmation.revision },
        cache: "no-store",
        credentials: "same-origin",
        headers: { "X-Steward-CSRF": csrf },
        path: { subject_id: confirmation.subjectId, user_id: member.userId },
      });
      response = result.response;
      if (response?.ok) {
        setConfirmation(null);
        setBusy(false);
        onChanged();
        return;
      }
    } else {
      const result = await associateAdminFederatedSubject({
        body: { canonicalUserId: member.userId, expectedRevision: confirmation.subject.revision },
        cache: "no-store",
        credentials: "same-origin",
        headers: { "X-Steward-CSRF": csrf },
        path: { subject_id: confirmation.subject.subjectId },
      });
      response = result.response;
      if (result.data && response?.ok) {
        setConfirmation(null);
        setIdentityId("");
        setShowIdentityPicker(false);
        setBusy(false);
        onChanged();
        return;
      }
    }
    setBusy(false);
    setConfirmation(null);
    setMessage(mutationMessage(response?.status));
  }

  const selectedIdentity = data.unassociatedIdentities.find((subject) => subject.subjectId === identityId);
  const grantAdmin = confirmation?.kind === "administrator" && confirmation.action === "grant";
  const dialog = confirmation ? {
    confirmLabel: confirmation.kind === "administrator"
      ? grantAdmin ? "Grant administrator" : "Remove administrator"
      : confirmation.kind === "unlink" ? "Unlink identity" : "Link identity",
    description: confirmation.kind === "administrator"
      ? `${grantAdmin ? "Grant" : "Remove"} organization administrator access for ${member.displayEmail}?`
      : confirmation.kind === "unlink"
        ? `Unlink ${confirmation.label} from ${member.displayEmail}? The observed identity will remain available for a verified reassociation.`
        : `Link ${confirmation.subject.subject} to ${member.displayEmail}? Do this only after verifying the identity owner.`,
    title: confirmation.kind === "administrator"
      ? `${grantAdmin ? "Grant" : "Remove"} administrator access?`
      : confirmation.kind === "unlink" ? "Unlink this identity?" : "Link this identity?",
    tone: confirmation.kind === "link" || grantAdmin ? "primary" as const : "danger" as const,
  } : null;

  return (
    <>
      <nav aria-label="Breadcrumb" className="flex items-center gap-2 text-sm text-muted-ink"><Link className="hover:text-ink" href="/admin">Admin</Link><span aria-hidden="true">/</span><Link className="hover:text-ink" href="/admin/members">Members</Link><span aria-hidden="true">/</span><span aria-current="page" className="truncate text-ink">{member.displayEmail}</span></nav>
      <header className="flex flex-wrap items-center gap-4">
        <span className="flex size-13 shrink-0 items-center justify-center rounded-full bg-line-soft text-base font-bold text-muted-ink">{initials(member)}</span>
        <div className="min-w-0 space-y-1">
          <div className="flex flex-wrap items-center gap-2.5"><h1 className="text-[28px] font-semibold leading-[34px] tracking-[-0.02em]" id="page-title">{displayName(member)}</h1><MemberStatus state={member.state} /></div>
          <p className="text-sm text-muted-ink">{member.displayEmail} · {member.state === "pending" ? `Invited ${relativeDate(member.createdAt)}${member.invitedBy ? ` by ${member.invitedBy}` : ""}` : `Member since ${relativeDate(member.createdAt)}`}</p>
          <div className="font-mono text-[13px] text-faint-ink">{member.userId}</div>
        </div>
      </header>

      {member.state === "pending" ? <div className="flex gap-2.5 rounded-[10px] bg-warn-soft px-4 py-3 text-sm leading-5 text-warn"><span className="mt-1.5 size-2 shrink-0 rounded-full bg-current" /><span>Invitation pending. This member becomes active after an exact verified organization sign-in as {member.displayEmail}. Access and roles below apply from then.</span></div> : null}

      <div className="rounded-panel border bg-panel">
        <DetailSection description="Manages members, templates and requests, and sees all runs. Administrators keep their user workspace." title="Administrator">
          <div className="space-y-2">
            <button
              aria-checked={member.administrator}
              className="flex items-center gap-3 text-left disabled:cursor-not-allowed disabled:opacity-50"
              disabled={busy || self}
              onClick={() => setConfirmation({ kind: "administrator", action: member.administrator ? "revoke" : "grant" })}
              role="switch"
              title={self ? "You can't remove your own administrator access." : undefined}
              type="button"
            >
              <span className={`relative h-[22px] w-[38px] shrink-0 rounded-full transition-colors ${member.administrator ? "bg-brand" : "bg-field"}`}><span className={`absolute left-[3px] top-[3px] size-4 rounded-full bg-white shadow transition-transform ${member.administrator ? "translate-x-4" : ""}`} /></span>
              <span className="text-sm font-semibold">{member.administrator ? "Administrator access on" : "Administrator access off"}</span>
            </button>
            {self ? <p className="text-[13px] text-muted-ink">You can&apos;t remove your own administrator access.</p> : null}
          </div>
        </DetailSection>
        <DetailSection description="Each role maps to an envelope template. The member can request envelopes against these templates." title="Member roles">
          <div className="space-y-2">
            <TagSelect addPlaceholder="Add another role…" disabled={busy} emptyPlaceholder="Search roles…" label="Member roles" onChange={(next) => void changeRoles(next)} options={roleOptions} value={member.memberRoles} />
            {member.memberRoles.filter((role) => !data.roleOptions.includes(role)).map((role) => <p className="text-xs text-warn" key={role}><span className="font-mono">{role}</span> · No template currently publishes this role.</p>)}
          </div>
        </DetailSection>
        <DetailSection description="External identities that resolve to this member, such as a GitHub Actions actor. Link one only after verifying its owner." title="Federated identities">
          <div className="space-y-2.5">
            {member.identities.map((identity) => <div className="flex items-center gap-3 rounded-[10px] border px-3.5 py-3" key={identity.subjectId}>
              <div className="min-w-0 flex-1 space-y-0.5"><p className="truncate font-mono text-[15px] font-semibold">{identity.subject}</p><p className="truncate font-mono text-[13px] text-muted-ink">{identity.issuer}</p><p className="text-xs text-faint-ink">Linked {relativeDate(identity.linkedAt)} by {identity.linkedBy}</p></div>
              <button className="h-9 shrink-0 rounded-control border border-err px-3 text-[13px] font-semibold text-err disabled:opacity-50" disabled={busy} onClick={() => setConfirmation({ kind: "unlink", label: identity.subject, revision: identity.revision, subjectId: identity.subjectId })} type="button">Unlink</button>
            </div>)}
            {member.identities.length === 0 ? <p className="text-sm text-muted-ink">No linked identities.</p> : null}
            {showIdentityPicker ? <div className="flex flex-wrap gap-2 rounded-[10px] bg-subtle p-3">
              <select aria-label="Observed identity" className="h-10 min-w-0 flex-1 rounded-control border bg-panel px-3 text-sm" onChange={(event) => setIdentityId(event.target.value)} value={identityId}><option value="">Choose observed identity</option>{data.unassociatedIdentities.map((subject) => <option key={subject.subjectId} value={subject.subjectId}>{subject.actorLogin ?? subject.subject} · {subject.subject}</option>)}</select>
              <button className="h-10 rounded-control bg-brand px-3 text-sm font-semibold text-on-brand disabled:opacity-50" disabled={!selectedIdentity || busy} onClick={() => { if (selectedIdentity) setConfirmation({ kind: "link", subject: selectedIdentity }); }} type="button">Link</button>
              <button className="h-10 rounded-control border px-3 text-sm font-semibold" onClick={() => { setShowIdentityPicker(false); setIdentityId(""); }} type="button">Cancel</button>
            </div> : <button className="h-9 rounded-control border px-3.5 text-[13px] font-semibold disabled:opacity-50" disabled={data.unassociatedIdentities.length === 0 || busy} onClick={() => setShowIdentityPicker(true)} type="button">Link identity</button>}
            {data.unassociatedIdentities.length === 0 && !showIdentityPicker ? <p className="text-xs text-faint-ink">No unassociated identities are available.</p> : null}
          </div>
        </DetailSection>
      </div>
      {message ? <p className="text-sm text-err" role="alert">{message}</p> : null}
      {dialog ? <ConfirmationDialog {...dialog} onConfirm={() => void confirmChange()} onOpenChange={(open) => { if (!open) setConfirmation(null); }} open pending={busy} /> : null}
    </>
  );
}

function DetailSection({ children, description, title }: Readonly<{ children: React.ReactNode; description: string; title: string }>) {
  return <section className="grid grid-cols-[repeat(auto-fit,minmax(260px,1fr))] gap-x-8 gap-y-3.5 border-t border-line-soft p-6 first:border-t-0"><div><h2 className="text-[15px] font-semibold">{title}</h2><p className="mt-1 text-[13px] leading-[19px] text-muted-ink">{description}</p></div><div>{children}</div></section>;
}
