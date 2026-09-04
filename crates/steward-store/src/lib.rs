//! Append-only operational history and approval-queue persistence.

use std::error::Error;
use std::fmt;

use sqlx::types::Json;
use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use steward_admission::{
    AdmissionDecision, AdmissionDelta, Envelope, EnvelopeScopeKind, EnvelopeSpec,
    envelope_is_within,
};
use steward_types::{
    AgentRuntimeSpec, CanonicalPrincipal, CanonicalUserId, Email, OrganizationId,
    OrganizationIdentity, OrganizationIdentityMigration, TaskExecutionBinding,
};
use uuid::Uuid;

#[derive(Clone)]
pub struct PgStore {
    pool: PgPool,
}

/// One immutable, administrator-published Workflow revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowRevisionRecord {
    pub name: String,
    pub version: i64,
    pub display_name: String,
    pub agent: String,
    pub prompt: String,
    pub content_digest: String,
    pub published_by: String,
    pub published_at: String,
}

/// Immutable Workflow content supplied to the persistence boundary.
pub struct WorkflowPublication<'a> {
    pub name: &'a str,
    pub display_name: &'a str,
    pub agent: &'a str,
    pub prompt: &'a str,
    pub content_digest: &'a str,
    pub published_by: &'a str,
}

/// The current Steward-local browser authorization for one opaque canonical user.
///
/// Google proves who a person is. This record proves only which Steward privileges an
/// operator has explicitly granted to that canonical user; it deliberately has no email,
/// issuer, provider-token, or cloud-provider input.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BrowserRbacAssignments {
    pub is_admin: bool,
    pub member_roles: Vec<String>,
}

impl BrowserRbacAssignments {
    fn from_active_assignments(
        assignments: impl IntoIterator<Item = BrowserRbacAssignment>,
    ) -> Self {
        let mut result = Self::default();
        for assignment in assignments {
            match assignment {
                BrowserRbacAssignment::Administrator => result.is_admin = true,
                BrowserRbacAssignment::MemberRole(member_role) => {
                    result.member_roles.push(member_role)
                }
            }
        }
        result.member_roles.sort();
        result.member_roles.dedup();
        result
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BrowserRbacAssignment {
    Administrator,
    MemberRole(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserRbacAssignmentAction {
    Grant,
    Revoke,
}

impl BrowserRbacAssignmentAction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Grant => "grant",
            Self::Revoke => "revoke",
        }
    }
}

pub struct BrowserRbacAssignmentChange<'a> {
    pub user_id: &'a CanonicalUserId,
    pub assignment: &'a BrowserRbacAssignment,
    pub action: BrowserRbacAssignmentAction,
    pub actor: &'a str,
}

fn is_valid_member_role(member_role: &str) -> bool {
    let bytes = member_role.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
}

#[cfg(test)]
mod browser_rbac_tests {
    use super::{BrowserRbacAssignment, BrowserRbacAssignments};

    #[test]
    fn unassigned_canonical_user_has_no_implicit_steward_authority() {
        let assignments = BrowserRbacAssignments::from_active_assignments([]);
        assert_eq!(assignments.member_roles, Vec::<String>::new());
        assert!(
            !assignments.is_admin,
            "Google identity alone must not silently bootstrap a Steward administrator"
        );
    }

    #[test]
    fn active_assignments_are_explicit_and_member_roles_are_deduplicated() {
        let assignments = BrowserRbacAssignments::from_active_assignments([
            BrowserRbacAssignment::MemberRole("engineer".to_owned()),
            BrowserRbacAssignment::Administrator,
            BrowserRbacAssignment::MemberRole("engineer".to_owned()),
            BrowserRbacAssignment::MemberRole("analyst".to_owned()),
        ]);
        assert!(assignments.is_admin);
        assert_eq!(assignments.member_roles, ["analyst", "engineer"]);
    }
}

impl PgStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn connect(database_url: &str) -> Result<Self, StoreError> {
        PgPool::connect(database_url)
            .await
            .map(Self::new)
            .map_err(database_error)
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn migrate(&self) -> Result<(), StoreError> {
        sqlx::migrate!("../../migrations")
            .run(&self.pool)
            .await
            .map_err(|error| StoreError::Database(error.to_string()))
    }

    pub async fn list_latest_workflows(&self) -> Result<Vec<WorkflowRevisionRecord>, StoreError> {
        let rows = sqlx::query(
            "SELECT DISTINCT ON (name) name, version, display_name, agent, prompt, \
                    content_digest, published_by, \
                    to_char(published_at AT TIME ZONE 'UTC', \
                            'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS published_at \
             FROM workflow_revisions \
             ORDER BY name, version DESC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(database_error)?;
        rows.into_iter().map(workflow_revision_record).collect()
    }

    pub async fn workflow_revision(
        &self,
        name: &str,
        version: i64,
    ) -> Result<Option<WorkflowRevisionRecord>, StoreError> {
        if name.is_empty() || version <= 0 {
            return Err(StoreError::InvalidWorkflow);
        }
        let row = sqlx::query(
            "SELECT name, version, display_name, agent, prompt, content_digest, published_by, \
                    to_char(published_at AT TIME ZONE 'UTC', \
                            'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS published_at \
             FROM workflow_revisions WHERE name = $1 AND version = $2",
        )
        .bind(name)
        .bind(version)
        .fetch_optional(&self.pool)
        .await
        .map_err(database_error)?;
        row.map(workflow_revision_record).transpose()
    }

    pub async fn publish_initial_workflow(
        &self,
        publication: WorkflowPublication<'_>,
    ) -> Result<WorkflowRevisionRecord, StoreError> {
        self.publish_workflow(publication, false).await
    }

    pub async fn publish_next_workflow(
        &self,
        publication: WorkflowPublication<'_>,
    ) -> Result<WorkflowRevisionRecord, StoreError> {
        self.publish_workflow(publication, true).await
    }

    async fn publish_workflow(
        &self,
        publication: WorkflowPublication<'_>,
        next: bool,
    ) -> Result<WorkflowRevisionRecord, StoreError> {
        if !valid_workflow_publication(&publication) {
            return Err(StoreError::InvalidWorkflow);
        }
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("workflow:{}", publication.name))
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
        let current = sqlx::query_scalar::<_, Option<i64>>(
            "SELECT max(version) FROM workflow_revisions WHERE name = $1",
        )
        .bind(publication.name)
        .fetch_one(&mut *transaction)
        .await
        .map_err(database_error)?;
        let version = match (next, current) {
            (false, None) => 1,
            (false, Some(_)) => return Err(StoreError::WorkflowAlreadyExists),
            (true, None) => return Err(StoreError::WorkflowNotFound),
            (true, Some(version)) => version.checked_add(1).ok_or(StoreError::InvalidWorkflow)?,
        };
        let row = sqlx::query(
            "INSERT INTO workflow_revisions \
             (name, version, display_name, agent, prompt, content_digest, published_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             RETURNING name, version, display_name, agent, prompt, content_digest, published_by, \
                       to_char(published_at AT TIME ZONE 'UTC', \
                               'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS published_at",
        )
        .bind(publication.name)
        .bind(version)
        .bind(publication.display_name)
        .bind(publication.agent)
        .bind(publication.prompt)
        .bind(publication.content_digest)
        .bind(publication.published_by)
        .fetch_one(&mut *transaction)
        .await
        .map_err(database_error)?;
        transaction.commit().await.map_err(database_error)?;
        workflow_revision_record(row)
    }

    /// Read the latest append-only local RBAC decisions for this exact canonical user.
    ///
    /// Missing rows deliberately mean no elevated authority. The database query is keyed only by
    /// the opaque canonical ID; email and external-provider claims are never authorization keys.
    pub async fn browser_rbac_assignments(
        &self,
        user_id: &CanonicalUserId,
    ) -> Result<BrowserRbacAssignments, StoreError> {
        let rows = sqlx::query(
            "WITH latest AS ( \
                SELECT DISTINCT ON (assignment_kind, member_role) \
                       assignment_kind, member_role, action \
                FROM browser_rbac_assignment_events \
                WHERE user_id = $1 \
                ORDER BY assignment_kind, member_role, at DESC, id DESC \
             ) \
             SELECT assignment_kind, member_role \
             FROM latest \
             WHERE action = 'grant'",
        )
        .bind(user_id.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(database_error)?;
        let mut assignments = Vec::with_capacity(rows.len());
        for row in rows {
            let assignment_kind: String = row.try_get("assignment_kind").map_err(database_error)?;
            match assignment_kind.as_str() {
                "administrator" => assignments.push(BrowserRbacAssignment::Administrator),
                "member_role" => {
                    let member_role: String = row.try_get("member_role").map_err(database_error)?;
                    if !is_valid_member_role(&member_role) {
                        return Err(StoreError::InvalidBrowserRbacRecord);
                    }
                    assignments.push(BrowserRbacAssignment::MemberRole(member_role));
                }
                _ => return Err(StoreError::InvalidBrowserRbacRecord),
            }
        }
        Ok(BrowserRbacAssignments::from_active_assignments(assignments))
    }

    /// Append an audited local RBAC grant or revocation. Existing events are immutable; an
    /// operator revokes authority by appending a new revocation event instead of editing history.
    pub async fn append_browser_rbac_assignment(
        &self,
        change: BrowserRbacAssignmentChange<'_>,
    ) -> Result<(), StoreError> {
        if change.actor.trim().is_empty() {
            return Err(StoreError::InvalidBrowserRbacActor);
        }
        let (assignment_kind, member_role) = match change.assignment {
            BrowserRbacAssignment::Administrator => ("administrator", None),
            BrowserRbacAssignment::MemberRole(member_role) if is_valid_member_role(member_role) => {
                ("member_role", Some(member_role))
            }
            BrowserRbacAssignment::MemberRole(_) => {
                return Err(StoreError::InvalidBrowserRbacAssignment);
            }
        };
        sqlx::query(
            "INSERT INTO browser_rbac_assignment_events \
             (id, user_id, assignment_kind, member_role, action, actor) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(Uuid::new_v4())
        .bind(change.user_id.as_str())
        .bind(assignment_kind)
        .bind(member_role)
        .bind(change.action.as_str())
        .bind(change.actor)
        .execute(&self.pool)
        .await
        .map_err(database_error)?;
        Ok(())
    }

    /// Resolve only an already-reviewed exact issuer/subject/organization mapping.
    ///
    /// Email is checked for staleness but is never used to discover or adopt a user.
    pub async fn resolve_canonical_identity(
        &self,
        identity: &OrganizationIdentity,
    ) -> Result<CanonicalPrincipal, StoreError> {
        self.resolve_canonical_identity_fields(
            identity.issuer(),
            identity.subject(),
            identity.organization_claim(),
            identity.organization_id(),
            identity.verified_email(),
        )
        .await
    }

    /// Resolve an alternative issuer only through its distinct reviewed migration proof.
    ///
    /// This read path does not turn the migration into a normal registration capability.
    pub async fn resolve_migrated_canonical_identity(
        &self,
        migration: &OrganizationIdentityMigration,
    ) -> Result<CanonicalPrincipal, StoreError> {
        self.resolve_canonical_identity_fields(
            migration.issuer(),
            migration.subject(),
            migration.organization_claim(),
            migration.organization_id(),
            migration.verified_email(),
        )
        .await
    }

    async fn resolve_canonical_identity_fields(
        &self,
        issuer: &str,
        subject: &str,
        organization_claim: &str,
        organization_id: &OrganizationId,
        verified_email: &Email,
    ) -> Result<CanonicalPrincipal, StoreError> {
        let row = sqlx::query(
            "SELECT canonical_users.user_id, \
                    canonical_users.organization_id AS user_organization_id, \
                    canonical_users.display_email, canonical_users.state, \
                    canonical_identity_subjects.verified_email, \
                    canonical_identity_subjects.organization_id AS subject_organization_id \
             FROM canonical_identity_subjects \
             JOIN canonical_users \
               ON canonical_users.user_id = canonical_identity_subjects.user_id \
             WHERE canonical_identity_subjects.issuer = $1 \
               AND canonical_identity_subjects.subject = $2 \
               AND canonical_identity_subjects.organization_claim = $3 \
               AND canonical_identity_subjects.organization_id = $4",
        )
        .bind(issuer)
        .bind(subject)
        .bind(organization_claim)
        .bind(organization_id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(database_error)?
        .ok_or(StoreError::CanonicalIdentityNotFound)?;

        canonical_principal_from_row(&row, organization_id, verified_email)
    }

    /// Resolve a trusted canonical-user reference and current display email.
    ///
    /// This is the bounded lookup used after a workload identity mapper has emitted
    /// an opaque user ID. It never accepts issuer claims or discovers a user by email.
    pub async fn resolve_canonical_principal(
        &self,
        user_id: &CanonicalUserId,
        current_verified_email: &Email,
    ) -> Result<CanonicalPrincipal, StoreError> {
        let row = sqlx::query(
            "SELECT organization_id, display_email, state \
             FROM canonical_users WHERE user_id = $1",
        )
        .bind(user_id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(database_error)?
        .ok_or(StoreError::CanonicalIdentityNotFound)?;
        let state: String = row.try_get("state").map_err(database_error)?;
        if state != "active" {
            return Err(StoreError::CanonicalIdentityInactive);
        }
        let display_email: String = row.try_get("display_email").map_err(database_error)?;
        if !display_email.eq_ignore_ascii_case(current_verified_email.as_str()) {
            return Err(StoreError::CanonicalIdentityStale);
        }
        let organization_id = row
            .try_get::<String, _>("organization_id")
            .map_err(database_error)
            .and_then(|value| {
                steward_types::OrganizationId::parse(value)
                    .map_err(|_| StoreError::CanonicalIdentityInvalidRecord)
            })?;
        CanonicalPrincipal::new(user_id.clone(), organization_id, Email(display_email))
            .map_err(|_| StoreError::CanonicalIdentityInvalidRecord)
    }

    /// Register a new person and exact external subject in one transaction.
    ///
    /// An email match never adopts an existing person. Repeated exact registration is
    /// idempotent only while every reviewed claim still matches.
    ///
    /// An explicitly reviewed issuer migration is not a normal-registration proof:
    ///
    /// ```compile_fail
    /// # use steward_store::PgStore;
    /// # use steward_types::{Email, OrganizationId, OrganizationIdentityMigration};
    /// # async fn cannot_register_migration(store: &PgStore) {
    /// let migration = OrganizationIdentityMigration::new_reviewed(
    ///     "https://login.example.test",
    ///     "immutable-subject",
    ///     "example.com",
    ///     OrganizationId::parse("org_example").unwrap(),
    ///     Email::parse("person@example.com").unwrap(),
    /// ).unwrap();
    /// store
    ///     .register_canonical_identity(migration.identity(), "identity-admin")
    ///     .await
    ///     .unwrap();
    /// # }
    /// ```
    pub async fn register_canonical_identity(
        &self,
        identity: &OrganizationIdentity,
        actor: &str,
    ) -> Result<CanonicalPrincipal, StoreError> {
        if actor.trim().is_empty() {
            return Err(StoreError::CanonicalIdentityInvalidActor);
        }
        match self.resolve_canonical_identity(identity).await {
            Ok(principal) => return Ok(principal),
            Err(StoreError::CanonicalIdentityNotFound) => {}
            Err(error) => return Err(error),
        }

        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        // Serialize both registration and migration attachment on the external pair. The
        // database uniqueness constraint remains the final concurrency boundary; this lock
        // additionally makes concurrent exact retries converge idempotently.
        sqlx::query(
            "SELECT pg_advisory_xact_lock(\
                hashtextextended($1::text || chr(31) || $2::text, 0)\
             )",
        )
        .bind(identity.issuer())
        .bind(identity.subject())
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        let existing_pair = sqlx::query(
            "SELECT canonical_users.user_id, canonical_users.display_email, \
                    canonical_users.state, \
                    canonical_users.organization_id AS user_organization_id, \
                    canonical_identity_subjects.verified_email, \
                    canonical_identity_subjects.organization_claim, \
                    canonical_identity_subjects.organization_id AS subject_organization_id \
             FROM canonical_identity_subjects \
             JOIN canonical_users \
               ON canonical_users.user_id = canonical_identity_subjects.user_id \
             WHERE canonical_identity_subjects.issuer = $1 \
               AND canonical_identity_subjects.subject = $2",
        )
        .bind(identity.issuer())
        .bind(identity.subject())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?;
        if let Some(row) = existing_pair {
            let organization_claim: String =
                row.try_get("organization_claim").map_err(database_error)?;
            let organization_id: String = row
                .try_get("subject_organization_id")
                .map_err(database_error)?;
            if organization_claim != identity.organization_claim()
                || organization_id != identity.organization_id().as_str()
            {
                return Err(StoreError::CanonicalIdentityConflict);
            }
            return canonical_principal_from_row(
                &row,
                identity.organization_id(),
                identity.verified_email(),
            );
        }
        let email_owner = sqlx::query_scalar::<_, String>(
            "SELECT user_id FROM canonical_users \
             WHERE organization_id = $1 AND lower(display_email) = lower($2)",
        )
        .bind(identity.organization_id().as_str())
        .bind(identity.verified_email().as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?;
        if email_owner.is_some() {
            return Err(StoreError::CanonicalIdentityAmbiguousEmail);
        }

        let user_id = CanonicalUserId::parse(format!("usr_{}", Uuid::new_v4().simple()))
            .map_err(|_| StoreError::CanonicalIdentityInvalidRecord)?;
        sqlx::query(
            "INSERT INTO canonical_users (user_id, organization_id, display_email) \
             VALUES ($1, $2, $3)",
        )
        .bind(user_id.as_str())
        .bind(identity.organization_id().as_str())
        .bind(identity.verified_email().as_str())
        .execute(&mut *transaction)
        .await
        .map_err(canonical_identity_database_error)?;
        sqlx::query(
            "INSERT INTO canonical_identity_subjects \
             (issuer, subject, organization_claim, organization_id, user_id, verified_email) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(identity.issuer())
        .bind(identity.subject())
        .bind(identity.organization_claim())
        .bind(identity.organization_id().as_str())
        .bind(user_id.as_str())
        .bind(identity.verified_email().as_str())
        .execute(&mut *transaction)
        .await
        .map_err(canonical_identity_database_error)?;
        sqlx::query(
            "INSERT INTO canonical_identity_audit \
             (id, user_id, action, actor, new_display_email) \
             VALUES ($1, $2, 'registered', $3, $4)",
        )
        .bind(Uuid::new_v4())
        .bind(user_id.as_str())
        .bind(actor)
        .bind(identity.verified_email().as_str())
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        transaction.commit().await.map_err(database_error)?;

        CanonicalPrincipal::new(
            user_id,
            identity.organization_id().clone(),
            identity.verified_email().clone(),
        )
        .map_err(|_| StoreError::CanonicalIdentityInvalidRecord)
    }

    /// Attach a newly reviewed external issuer/subject to an existing person.
    ///
    /// This is the only issuer-migration path: callers must name the opaque user ID,
    /// organization and current verified email explicitly. Email is never used to
    /// discover the target user.
    pub async fn attach_canonical_identity_subject(
        &self,
        user_id: &CanonicalUserId,
        migration: &OrganizationIdentityMigration,
        actor: &str,
    ) -> Result<CanonicalPrincipal, StoreError> {
        let identity = migration;
        if actor.trim().is_empty() {
            return Err(StoreError::CanonicalIdentityInvalidActor);
        }
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        sqlx::query(
            "SELECT pg_advisory_xact_lock(\
                hashtextextended($1::text || chr(31) || $2::text, 0)\
             )",
        )
        .bind(identity.issuer())
        .bind(identity.subject())
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        let user = sqlx::query(
            "SELECT organization_id, display_email, state FROM canonical_users WHERE user_id = $1",
        )
        .bind(user_id.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?
        .ok_or(StoreError::CanonicalIdentityNotFound)?;
        let organization_id: String = user.try_get("organization_id").map_err(database_error)?;
        let stored_organization_id = OrganizationId::parse(organization_id.clone())
            .map_err(|_| StoreError::CanonicalIdentityInvalidRecord)?;
        let display_email: String = user.try_get("display_email").map_err(database_error)?;
        let state: String = user.try_get("state").map_err(database_error)?;
        if state != "active" {
            return Err(StoreError::CanonicalIdentityInactive);
        }
        if organization_id != identity.organization_id().as_str()
            || !display_email.eq_ignore_ascii_case(identity.verified_email().as_str())
        {
            return Err(StoreError::CanonicalIdentityStale);
        }

        let existing = sqlx::query(
            "SELECT user_id, organization_claim, organization_id, verified_email \
             FROM canonical_identity_subjects WHERE issuer = $1 AND subject = $2",
        )
        .bind(identity.issuer())
        .bind(identity.subject())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?;
        if let Some(existing) = existing {
            let existing_user_id: String = existing.try_get("user_id").map_err(database_error)?;
            let existing_organization_claim: String = existing
                .try_get("organization_claim")
                .map_err(database_error)?;
            let existing_organization_id: String = existing
                .try_get("organization_id")
                .map_err(database_error)?;
            let existing_verified_email: String =
                existing.try_get("verified_email").map_err(database_error)?;
            if existing_user_id != user_id.as_str()
                || existing_organization_claim != identity.organization_claim()
                || existing_organization_id != identity.organization_id().as_str()
            {
                return Err(StoreError::CanonicalIdentityConflict);
            }
            if !existing_verified_email.eq_ignore_ascii_case(identity.verified_email().as_str()) {
                return Err(StoreError::CanonicalIdentityStale);
            }
            return CanonicalPrincipal::new(
                user_id.clone(),
                stored_organization_id,
                Email(display_email),
            )
            .map_err(|_| StoreError::CanonicalIdentityInvalidRecord);
        }

        sqlx::query(
            "INSERT INTO canonical_identity_subjects \
             (issuer, subject, organization_claim, organization_id, user_id, verified_email) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(identity.issuer())
        .bind(identity.subject())
        .bind(identity.organization_claim())
        .bind(identity.organization_id().as_str())
        .bind(user_id.as_str())
        .bind(identity.verified_email().as_str())
        .execute(&mut *transaction)
        .await
        .map_err(canonical_identity_database_error)?;
        sqlx::query(
            "INSERT INTO canonical_identity_audit (id, user_id, action, actor) \
             VALUES ($1, $2, 'identity_attached', $3)",
        )
        .bind(Uuid::new_v4())
        .bind(user_id.as_str())
        .bind(actor)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        transaction.commit().await.map_err(database_error)?;

        CanonicalPrincipal::new(
            user_id.clone(),
            stored_organization_id,
            Email(display_email),
        )
        .map_err(|_| StoreError::CanonicalIdentityInvalidRecord)
    }

    /// Apply an explicitly reviewed email rename without changing the immutable user ID.
    pub async fn change_canonical_identity_email(
        &self,
        user_id: &CanonicalUserId,
        expected_previous_email: &Email,
        new_verified_email: &Email,
        actor: &str,
    ) -> Result<(), StoreError> {
        if actor.trim().is_empty() {
            return Err(StoreError::CanonicalIdentityInvalidActor);
        }
        Email::parse(new_verified_email.0.clone())
            .map_err(|_| StoreError::CanonicalIdentityInvalidRecord)?;
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let updated = sqlx::query(
            "UPDATE canonical_users \
             SET display_email = $1, state = 'active', updated_at = now() \
             WHERE user_id = $2 AND lower(display_email) = lower($3)",
        )
        .bind(&new_verified_email.0)
        .bind(user_id.as_str())
        .bind(&expected_previous_email.0)
        .execute(&mut *transaction)
        .await
        .map_err(canonical_identity_database_error)?;
        if updated.rows_affected() != 1 {
            return Err(StoreError::CanonicalIdentityStale);
        }
        sqlx::query(
            "UPDATE canonical_identity_subjects \
             SET verified_email = $1, updated_at = now() WHERE user_id = $2",
        )
        .bind(&new_verified_email.0)
        .bind(user_id.as_str())
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        sqlx::query(
            "INSERT INTO canonical_identity_audit \
             (id, user_id, action, actor, previous_display_email, new_display_email) \
             VALUES ($1, $2, 'email_changed', $3, $4, $5)",
        )
        .bind(Uuid::new_v4())
        .bind(user_id.as_str())
        .bind(actor)
        .bind(&expected_previous_email.0)
        .bind(&new_verified_email.0)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        transaction.commit().await.map_err(database_error)
    }

    pub async fn record_spend_observation(
        &self,
        runtime_uid: &str,
        observed_generation: i64,
        spec_digest: &str,
        spend: &steward_types::SpendSummary,
        exhausted: bool,
    ) -> Result<(), StoreError> {
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        if exhausted {
            sqlx::query(
                "INSERT INTO inference_exhaustions \
                 (runtime_uid, observed_generation, spec_digest, observed_amount, currency) \
                 VALUES ($1, $2, $3, $4::numeric, $5)",
            )
            .bind(runtime_uid)
            .bind(observed_generation)
            .bind(spec_digest)
            .bind(&spend.observed_amount)
            .bind(&spend.currency)
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
        }
        sqlx::query(
            "INSERT INTO spend_observations \
             (runtime_uid, observed_amount, currency, exhausted) \
             VALUES ($1, $2::numeric, $3, $4)",
        )
        .bind(runtime_uid)
        .bind(&spend.observed_amount)
        .bind(&spend.currency)
        .bind(exhausted)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        transaction.commit().await.map_err(database_error)
    }

    pub async fn inference_exhaustion(
        &self,
        runtime_uid: &str,
    ) -> Result<Option<steward_types::SpendSummary>, StoreError> {
        sqlx::query(
            "SELECT observed_amount::text AS observed_amount, currency \
             FROM inference_exhaustions \
             WHERE runtime_uid = $1 \
             ORDER BY at DESC, id DESC \
             LIMIT 1",
        )
        .bind(runtime_uid)
        .fetch_optional(&self.pool)
        .await
        .map_err(database_error)?
        .map(|row| {
            Ok(steward_types::SpendSummary {
                observed_amount: row.try_get("observed_amount").map_err(database_error)?,
                currency: row.try_get("currency").map_err(database_error)?,
            })
        })
        .transpose()
    }

    pub async fn agent_runs(&self, query: &AgentRunQuery) -> Result<AgentRunPage, StoreError> {
        if query.limit == 0 || query.limit > 100 {
            return Err(StoreError::InvalidRunQuery);
        }
        if query
            .workflow
            .as_deref()
            .is_some_and(|workflow| workflow.is_empty())
        {
            return Err(StoreError::InvalidRunQuery);
        }
        if query
            .runtime_uid
            .as_deref()
            .is_some_and(|runtime_uid| runtime_uid.is_empty())
        {
            return Err(StoreError::InvalidRunQuery);
        }
        if query
            .user_envelope_instance_id
            .as_deref()
            .is_some_and(|instance_id| instance_id.is_empty())
        {
            return Err(StoreError::InvalidRunQuery);
        }
        if let Some(cursor) = query.cursor {
            let mut cursor_exists = QueryBuilder::<Postgres>::new(
                "SELECT EXISTS(SELECT 1 FROM task_submissions tasks WHERE task_uid = ",
            );
            cursor_exists.push_bind(cursor);
            cursor_exists.push(
                " AND NOT EXISTS (SELECT 1 FROM connection_operations operations \
                   WHERE operations.task_uid = tasks.task_uid)",
            );
            if let Some(owner_user_id) = query.owner_user_id.as_deref() {
                cursor_exists.push(" AND owner_user_id = ");
                cursor_exists.push_bind(owner_user_id);
            }
            cursor_exists.push(")");
            let exists = cursor_exists
                .build_query_scalar::<bool>()
                .fetch_one(&self.pool)
                .await
                .map_err(database_error)?;
            if !exists {
                return Err(StoreError::InvalidRunCursor);
            }
        }

        let mut statement = QueryBuilder::<Postgres>::new(AGENT_RUN_SELECT);
        statement.push(
            " WHERE NOT EXISTS (SELECT 1 FROM connection_operations operations \
               WHERE operations.task_uid = tasks.task_uid)",
        );
        if let Some(cursor) = query.cursor {
            statement.push(
                " AND (tasks.created_at, tasks.task_uid) < \
                 (SELECT created_at, task_uid FROM task_submissions WHERE task_uid = ",
            );
            statement.push_bind(cursor);
            statement.push(")");
        }
        if let Some(phase) = query.phase {
            statement.push(" AND tasks.phase = ");
            statement.push_bind(task_phase_text(phase));
        }
        if let Some(workflow) = query.workflow.as_deref() {
            statement.push(" AND tasks.workflow = ");
            statement.push_bind(workflow);
        }
        if let Some(owner_user_id) = query.owner_user_id.as_deref() {
            statement.push(" AND tasks.owner_user_id = ");
            statement.push_bind(owner_user_id);
        }
        if let Some(runtime_uid) = query.runtime_uid.as_deref() {
            statement.push(" AND tasks.runtime_uid = ");
            statement.push_bind(runtime_uid);
        }
        if let Some(instance_id) = query.user_envelope_instance_id.as_deref() {
            statement.push(" AND tasks.user_envelope_instance_id = ");
            statement.push_bind(instance_id);
        }
        if let Some(task_uid) = query.task_uid {
            statement.push(" AND tasks.task_uid = ");
            statement.push_bind(task_uid);
        }
        statement.push(" ORDER BY tasks.created_at DESC, tasks.task_uid DESC LIMIT ");
        statement.push_bind(i64::from(query.limit) + 1);

        let mut records = statement
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(database_error)?
            .into_iter()
            .map(agent_run_record)
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = if records.len() > query.limit as usize {
            records.truncate(query.limit as usize);
            records.last().map(|record| record.task_uid)
        } else {
            None
        };
        Ok(AgentRunPage {
            records,
            next_cursor,
        })
    }

    pub async fn agent_run(&self, task_uid: Uuid) -> Result<Option<AgentRunRecord>, StoreError> {
        let mut statement = QueryBuilder::<Postgres>::new(AGENT_RUN_SELECT);
        statement.push(
            " WHERE NOT EXISTS (SELECT 1 FROM connection_operations operations \
               WHERE operations.task_uid = tasks.task_uid) AND tasks.task_uid = ",
        );
        statement.push_bind(task_uid);
        statement
            .build()
            .fetch_optional(&self.pool)
            .await
            .map_err(database_error)?
            .map(agent_run_record)
            .transpose()
    }

    pub async fn agent_run_timeline(
        &self,
        task_uid: Uuid,
    ) -> Result<Option<Vec<AgentRunTimelineEvent>>, StoreError> {
        if !sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM task_submissions tasks \
             WHERE tasks.task_uid = $1 \
               AND NOT EXISTS (SELECT 1 FROM connection_operations operations \
                   WHERE operations.task_uid = tasks.task_uid))",
        )
        .bind(task_uid)
        .fetch_one(&self.pool)
        .await
        .map_err(database_error)?
        {
            return Ok(None);
        }
        sqlx::query(
            "SELECT event_kind, phase, provenance, \
                    to_char(at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS at \
             FROM task_lifecycle_events \
             WHERE task_uid = $1 \
             ORDER BY at, id",
        )
        .bind(task_uid)
        .fetch_all(&self.pool)
        .await
        .map_err(database_error)?
        .into_iter()
        .map(agent_run_timeline_event)
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
    }

    /// Reserve an immutable user-envelope request under the server-resolved canonical owner.
    ///
    /// The initial `pending` event is written in the same transaction as the request. Callers
    /// may only advance the request through `append_envelope_request_status`; neither the
    /// browser nor this table ever overwrites a current status.
    pub async fn reserve_envelope_request(
        &self,
        request: EnvelopeRequestReservationRequest<'_>,
    ) -> Result<EnvelopeRequestReservation, StoreError> {
        if request.template_id.trim().is_empty()
            || request.idempotency_key.trim().is_empty()
            || request.actor.trim().is_empty()
        {
            return Err(StoreError::InvalidEnvelopeRequest);
        }
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!(
                "envelope-request:{}:{}",
                request.owner_user_id.as_str(),
                request.idempotency_key
            ))
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
        let existing = sqlx::query(
            "SELECT id, template_id, template_revision, requested_envelope \
             FROM envelope_requests \
             WHERE owner_user_id = $1 AND idempotency_key = $2",
        )
        .bind(request.owner_user_id.as_str())
        .bind(request.idempotency_key)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?;
        if let Some(row) = existing {
            let id: Uuid = row.try_get("id").map_err(database_error)?;
            let template_id: String = row.try_get("template_id").map_err(database_error)?;
            let template_revision: i64 =
                row.try_get("template_revision").map_err(database_error)?;
            let requested_envelope = row
                .try_get::<Json<Envelope>, _>("requested_envelope")
                .map_err(database_error)?
                .0;
            if template_id != request.template_id
                || template_revision != request.template_revision
                || requested_envelope != *request.requested_envelope
            {
                return Err(StoreError::EnvelopeRequestIdempotencyConflict);
            }
            transaction.commit().await.map_err(database_error)?;
            let record = self
                .envelope_request(request.owner_user_id, id)
                .await?
                .ok_or(StoreError::EnvelopeRequestNotFound)?;
            return Ok(EnvelopeRequestReservation {
                inserted: false,
                record,
            });
        }

        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO envelope_requests \
             (id, owner_user_id, template_id, template_revision, requested_envelope, idempotency_key) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id)
        .bind(request.owner_user_id.as_str())
        .bind(request.template_id)
        .bind(request.template_revision)
        .bind(Json(request.requested_envelope))
        .bind(request.idempotency_key)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        sqlx::query(
            "INSERT INTO envelope_request_events \
             (request_id, status, actor, template_revision) \
             VALUES ($1, 'pending', $2, $3)",
        )
        .bind(id)
        .bind(request.actor)
        .bind(request.template_revision)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        transaction.commit().await.map_err(database_error)?;
        let record = self
            .envelope_request(request.owner_user_id, id)
            .await?
            .ok_or(StoreError::EnvelopeRequestNotFound)?;
        Ok(EnvelopeRequestReservation {
            inserted: true,
            record,
        })
    }

    /// Read the current authoritative status derived from the latest immutable event, scoped to
    /// one canonical owner. An absent/mismatched owner is deliberately indistinguishable.
    pub async fn envelope_request(
        &self,
        owner_user_id: &CanonicalUserId,
        request_id: Uuid,
    ) -> Result<Option<EnvelopeRequestRecord>, StoreError> {
        let mut statement = QueryBuilder::<Postgres>::new(ENVELOPE_REQUEST_COLUMNS);
        statement.push("WHERE requests.owner_user_id = ");
        statement.push_bind(owner_user_id.as_str());
        statement.push(" AND requests.id = ");
        statement.push_bind(request_id);
        statement
            .build()
            .fetch_optional(&self.pool)
            .await
            .map_err(database_error)?
            .map(envelope_request_record)
            .transpose()
    }

    /// Read one envelope request for an already-authorized administrator.
    /// Browser authorization remains outside the store; this lookup intentionally has no owner
    /// parameter so an administrator never has to impersonate the request owner.
    pub async fn envelope_request_for_admin(
        &self,
        request_id: Uuid,
    ) -> Result<Option<EnvelopeRequestRecord>, StoreError> {
        let mut statement = QueryBuilder::<Postgres>::new(ENVELOPE_REQUEST_COLUMNS);
        statement.push("WHERE requests.id = ");
        statement.push_bind(request_id);
        statement
            .build()
            .fetch_optional(&self.pool)
            .await
            .map_err(database_error)?
            .map(envelope_request_record)
            .transpose()
    }

    /// List only the authenticated canonical owner's envelope requests.
    pub async fn envelope_requests(
        &self,
        owner_user_id: &CanonicalUserId,
    ) -> Result<Vec<EnvelopeRequestRecord>, StoreError> {
        let mut statement = QueryBuilder::<Postgres>::new(ENVELOPE_REQUEST_COLUMNS);
        statement.push("WHERE requests.owner_user_id = ");
        statement.push_bind(owner_user_id.as_str());
        statement.push(" ORDER BY requests.created_at DESC, requests.id DESC");
        statement
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(database_error)?
            .into_iter()
            .map(envelope_request_record)
            .collect()
    }

    /// List current pending user-envelope requests for the administrator approval queue.
    pub async fn pending_envelope_requests(
        &self,
    ) -> Result<Vec<PendingEnvelopeRequest>, StoreError> {
        let rows = sqlx::query(
            "SELECT requests.id, users.display_email AS owner_display_email, \
                    requests.template_id, requests.template_revision, \
                    requests.requested_envelope, templates.spec AS template_spec, \
                    to_char(requests.created_at AT TIME ZONE 'UTC', \
                            'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS created_at \
             FROM envelope_requests requests \
             JOIN canonical_users users ON users.user_id = requests.owner_user_id \
             JOIN envelopes templates \
               ON templates.scope_kind = 'member_role' \
              AND templates.scope_ref = requests.template_id \
              AND templates.revision = requests.template_revision \
             JOIN LATERAL ( \
                 SELECT events.status \
                 FROM envelope_request_events events \
                 WHERE events.request_id = requests.id \
                 ORDER BY events.at DESC, events.id DESC \
                 LIMIT 1 \
             ) status ON true \
             WHERE status.status = 'pending' \
             ORDER BY requests.created_at, requests.id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(database_error)?;
        rows.into_iter()
            .map(|row| {
                Ok(PendingEnvelopeRequest {
                    request_id: row.try_get("id").map_err(database_error)?,
                    owner_display_email: row
                        .try_get("owner_display_email")
                        .map_err(database_error)?,
                    template_id: row.try_get("template_id").map_err(database_error)?,
                    template_revision: row.try_get("template_revision").map_err(database_error)?,
                    requested_envelope: row
                        .try_get::<Json<Envelope>, _>("requested_envelope")
                        .map_err(database_error)?
                        .0,
                    template_envelope: Envelope {
                        revision: row.try_get("template_revision").map_err(database_error)?,
                        spec: row
                            .try_get::<Json<EnvelopeSpec>, _>("template_spec")
                            .map_err(database_error)?
                            .0,
                    },
                    created_at: row.try_get("created_at").map_err(database_error)?,
                })
            })
            .collect()
    }

    /// Append a server-side lifecycle transition after the approval/provisioning authority has
    /// made its decision. This API never accepts a browser session or caller-supplied owner.
    pub async fn append_envelope_request_status(
        &self,
        request_id: Uuid,
        update: EnvelopeRequestStatusUpdate<'_>,
    ) -> Result<EnvelopeRequestRecord, StoreError> {
        if !valid_envelope_request_transition(update.from, update.to)
            || update.actor.trim().is_empty()
        {
            return Err(StoreError::InvalidEnvelopeRequestTransition);
        }
        if update.to == EnvelopeRequestStatus::Provisioned
            && (update.envelope_instance_id.is_none() || update.envelope_digest.is_none())
        {
            return Err(StoreError::InvalidEnvelopeRequest);
        }
        if update.to != EnvelopeRequestStatus::Provisioned
            && (update.envelope_instance_id.is_some() || update.envelope_digest.is_some())
        {
            return Err(StoreError::InvalidEnvelopeRequest);
        }
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let request = sqlx::query(
            "SELECT owner_user_id, template_id, template_revision, requested_envelope \
             FROM envelope_requests WHERE id = $1 FOR UPDATE",
        )
        .bind(request_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?
        .ok_or(StoreError::EnvelopeRequestNotFound)?;
        let owner_user_id = request
            .try_get::<String, _>("owner_user_id")
            .map_err(database_error)
            .and_then(|value| {
                CanonicalUserId::parse(value)
                    .map_err(|_| StoreError::CanonicalIdentityInvalidRecord)
            })?;
        let template_id: String = request.try_get("template_id").map_err(database_error)?;
        let template_revision: i64 = request
            .try_get("template_revision")
            .map_err(database_error)?;
        let requested_envelope = request
            .try_get::<Json<Envelope>, _>("requested_envelope")
            .map_err(database_error)?
            .0;

        let latest = sqlx::query(
            "SELECT status FROM envelope_request_events \
             WHERE request_id = $1 ORDER BY at DESC, id DESC LIMIT 1 FOR UPDATE",
        )
        .bind(request_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?
        .ok_or(StoreError::EnvelopeRequestNotFound)?;
        let current = envelope_request_status_from_text(
            &latest
                .try_get::<String, _>("status")
                .map_err(database_error)?,
        )?;
        if current == update.to {
            transaction.commit().await.map_err(database_error)?;
            return self
                .envelope_request(&owner_user_id, request_id)
                .await?
                .ok_or(StoreError::EnvelopeRequestNotFound);
        }
        if current != update.from {
            return Err(StoreError::InvalidEnvelopeRequestTransition);
        }
        if update.to == EnvelopeRequestStatus::Provisioned {
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
                .bind(format!("active-user-envelope:{}", owner_user_id.as_str()))
                .execute(&mut *transaction)
                .await
                .map_err(database_error)?;
            lock_envelope_scope(
                &mut transaction,
                EnvelopeScopeKind::MemberRole,
                &template_id,
            )
            .await?;
            let current_revision = sqlx::query_scalar::<_, Option<i64>>(
                "SELECT max(revision) FROM envelopes \
                 WHERE scope_kind = 'member_role' AND scope_ref = $1",
            )
            .bind(&template_id)
            .fetch_one(&mut *transaction)
            .await
            .map_err(database_error)?;
            if current_revision != Some(template_revision) {
                return Err(StoreError::EnvelopeRequestTemplateStale);
            }
        }
        let needs_snapshot = matches!(
            update.to,
            EnvelopeRequestStatus::Approved | EnvelopeRequestStatus::Provisioned
        );
        match (needs_snapshot, update.approved_envelope) {
            (true, Some(approved_envelope))
                if update.to == EnvelopeRequestStatus::Provisioned
                    && *approved_envelope == requested_envelope => {}
            (true, Some(approved_envelope))
                if update.to == EnvelopeRequestStatus::Approved
                    && matches!(
                        envelope_is_within(approved_envelope, &requested_envelope),
                        Ok(AdmissionDecision::Admit)
                    ) => {}
            (false, None) => {}
            _ => return Err(StoreError::InvalidEnvelopeRequest),
        }
        if update.to == EnvelopeRequestStatus::Provisioned {
            sqlx::query(
                "INSERT INTO envelope_request_events \
                 (request_id, status, reason, actor, template_revision) \
                 SELECT requests.id, 'stale', $3, $2, requests.template_revision \
                 FROM envelope_requests requests \
                 JOIN LATERAL ( \
                     SELECT events.status \
                     FROM envelope_request_events events \
                     WHERE events.request_id = requests.id \
                     ORDER BY events.at DESC, events.id DESC \
                     LIMIT 1 \
                 ) current_status ON true \
                 WHERE requests.owner_user_id = $1 \
                   AND requests.id <> $4 \
                   AND current_status.status = 'provisioned'",
            )
            .bind(owner_user_id.as_str())
            .bind(update.actor)
            .bind(format!("superseded by envelope request {request_id}"))
            .bind(request_id)
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
        }
        sqlx::query(
            "INSERT INTO envelope_request_events \
             (request_id, status, approval_id, envelope_instance_id, envelope_digest, reason, \
              approved_envelope, actor, template_revision) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(request_id)
        .bind(update.to.as_str())
        .bind(update.approval_id)
        .bind(update.envelope_instance_id)
        .bind(update.envelope_digest)
        .bind(update.reason)
        .bind(update.approved_envelope.map(Json))
        .bind(update.actor)
        .bind(template_revision)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        transaction.commit().await.map_err(database_error)?;
        self.envelope_request(&owner_user_id, request_id)
            .await?
            .ok_or(StoreError::EnvelopeRequestNotFound)
    }

    pub async fn insert_envelope(
        &self,
        member_role: &str,
        envelope: &Envelope,
        authored_by: &str,
    ) -> Result<(), StoreError> {
        self.insert_scoped_envelope(
            EnvelopeScopeKind::MemberRole,
            member_role,
            envelope,
            authored_by,
        )
        .await
    }

    pub async fn insert_service_envelope(
        &self,
        service: &str,
        envelope: &Envelope,
        authored_by: &str,
    ) -> Result<(), StoreError> {
        self.insert_scoped_envelope(EnvelopeScopeKind::Service, service, envelope, authored_by)
            .await
    }

    async fn insert_scoped_envelope(
        &self,
        scope_kind: EnvelopeScopeKind,
        scope_ref: &str,
        envelope: &Envelope,
        authored_by: &str,
    ) -> Result<(), StoreError> {
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        lock_envelope_scope(&mut transaction, scope_kind, scope_ref).await?;
        let latest_revision = sqlx::query_scalar::<_, Option<i64>>(
            "SELECT max(revision) \
             FROM envelopes \
             WHERE scope_kind = $1 AND scope_ref = $2",
        )
        .bind(scope_kind.as_str())
        .bind(scope_ref)
        .fetch_one(&mut *transaction)
        .await
        .map_err(database_error)?;
        if latest_revision.is_some_and(|revision| envelope.revision <= revision) {
            return Err(StoreError::EnvelopeRevisionNotIncreasing);
        }
        sqlx::query(
            "INSERT INTO envelopes \
             (scope_kind, scope_ref, revision, spec, authored_by) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(scope_kind.as_str())
        .bind(scope_ref)
        .bind(envelope.revision)
        .bind(Json(&envelope.spec))
        .bind(authored_by)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        sqlx::query(
            "INSERT INTO grant_revocations (grant_id, revoked_by, reason) \
             SELECT grants.id, $3, 'envelope scope superseded' \
             FROM grants \
             JOIN approvals ON approvals.id = grants.approval_id \
             JOIN admission_decisions \
               ON admission_decisions.id = approvals.admission_decision_id \
             LEFT JOIN grant_revocations ON grant_revocations.grant_id = grants.id \
             WHERE admission_decisions.member_role = $1 \
               AND admission_decisions.proposed_spec->'principal'->>'kind' = $2 \
               AND admission_decisions.envelope_rev <> $4 \
               AND grant_revocations.grant_id IS NULL \
             ON CONFLICT (grant_id) DO NOTHING",
        )
        .bind(scope_ref)
        .bind(match scope_kind {
            EnvelopeScopeKind::MemberRole => "user",
            EnvelopeScopeKind::Service => "service",
        })
        .bind(authored_by)
        .bind(envelope.revision)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        transaction.commit().await.map_err(database_error)?;
        Ok(())
    }

    pub async fn latest_envelope(&self, member_role: &str) -> Result<Option<Envelope>, StoreError> {
        self.latest_scoped_envelope(EnvelopeScopeKind::MemberRole, member_role)
            .await
    }

    pub async fn latest_envelopes(&self) -> Result<Vec<(String, Envelope)>, StoreError> {
        let rows = sqlx::query(
            "SELECT DISTINCT ON (scope_ref) scope_ref, revision, spec \
             FROM envelopes \
             WHERE scope_kind = 'member_role' \
             ORDER BY scope_ref, revision DESC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(database_error)?;
        rows.into_iter()
            .map(|row| {
                let member_role = row.try_get("scope_ref").map_err(database_error)?;
                let revision = row.try_get("revision").map_err(database_error)?;
                let Json(spec) = row
                    .try_get::<Json<EnvelopeSpec>, _>("spec")
                    .map_err(database_error)?;
                Ok((member_role, Envelope { revision, spec }))
            })
            .collect()
    }

    pub async fn latest_service_envelope(
        &self,
        service: &str,
    ) -> Result<Option<Envelope>, StoreError> {
        self.latest_scoped_envelope(EnvelopeScopeKind::Service, service)
            .await
    }

    pub async fn latest_scoped_envelope(
        &self,
        scope_kind: EnvelopeScopeKind,
        scope_ref: &str,
    ) -> Result<Option<Envelope>, StoreError> {
        let row = sqlx::query(
            "SELECT revision, spec \
             FROM envelopes \
             WHERE scope_kind = $1 AND scope_ref = $2 \
             ORDER BY revision DESC \
             LIMIT 1",
        )
        .bind(scope_kind.as_str())
        .bind(scope_ref)
        .fetch_optional(&self.pool)
        .await
        .map_err(database_error)?;
        row.map(|row| {
            let revision = row.try_get("revision").map_err(database_error)?;
            let Json(spec) = row
                .try_get::<Json<EnvelopeSpec>, _>("spec")
                .map_err(database_error)?;
            Ok(Envelope { revision, spec })
        })
        .transpose()
    }

    pub async fn park_rejection(
        &self,
        request: ParkRejection<'_>,
    ) -> Result<ParkedAdmission, StoreError> {
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!(
                "{}:{}:{}:{}:{}:{}:{}",
                request.runtime_uid,
                request.spec_digest,
                request.envelope_revision,
                request.base_spec_digest,
                request.base_pending_approval_digest.unwrap_or_default(),
                request.actor,
                request.member_role,
            ))
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
        let existing = sqlx::query(
            "SELECT \
                admission_decisions.id AS decision_id, \
                approvals.id AS approval_id, \
                approvals.decision_key, \
                approvals.evidence_url \
             FROM admission_decisions \
             JOIN approvals ON approvals.admission_decision_id = admission_decisions.id \
             WHERE admission_decisions.runtime_uid = $1 \
               AND admission_decisions.spec_digest = $2 \
               AND admission_decisions.envelope_rev = $3 \
               AND admission_decisions.base_spec_digest = $4 \
               AND admission_decisions.actor = $5 \
               AND admission_decisions.member_role = $6 \
               AND admission_decisions.base_pending_approval_digest IS NOT DISTINCT FROM $7 \
               AND approvals.state = 'pending' \
             ORDER BY admission_decisions.at DESC \
             LIMIT 1",
        )
        .bind(request.runtime_uid)
        .bind(request.spec_digest)
        .bind(request.envelope_revision)
        .bind(request.base_spec_digest)
        .bind(request.actor)
        .bind(request.member_role)
        .bind(request.base_pending_approval_digest)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?;
        if let Some(row) = existing {
            let parked = ParkedAdmission {
                decision_id: row.try_get("decision_id").map_err(database_error)?,
                approval_id: row.try_get("approval_id").map_err(database_error)?,
                decision_key: row.try_get("decision_key").map_err(database_error)?,
                evidence_url: row.try_get("evidence_url").map_err(database_error)?,
            };
            transaction.commit().await.map_err(database_error)?;
            return Ok(parked);
        }

        let decision_id = Uuid::new_v4();
        let approval_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO admission_decisions \
             (id, runtime_uid, spec_digest, envelope_rev, verdict, deltas, proposed_spec, actor, \
              member_role, base_spec_digest, base_spec, runtime_namespace, runtime_name, \
              base_pending_approval_digest) \
             VALUES ($1, $2, $3, $4, 'reject', $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(decision_id)
        .bind(request.runtime_uid)
        .bind(request.spec_digest)
        .bind(request.envelope_revision)
        .bind(Json(request.deltas))
        .bind(Json(request.proposed_spec))
        .bind(request.actor)
        .bind(request.member_role)
        .bind(request.base_spec_digest)
        .bind(Json(request.base_spec))
        .bind(request.runtime_namespace)
        .bind(request.runtime_name)
        .bind(request.base_pending_approval_digest)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        sqlx::query(
            "INSERT INTO approvals \
             (id, runtime_uid, admission_decision_id, state) \
             VALUES ($1, $2, $3, 'pending')",
        )
        .bind(approval_id)
        .bind(request.runtime_uid)
        .bind(decision_id)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        transaction.commit().await.map_err(database_error)?;
        Ok(ParkedAdmission {
            decision_id,
            approval_id,
            decision_key: None,
            evidence_url: None,
        })
    }

    pub async fn pending_approvals(&self) -> Result<Vec<PendingApproval>, StoreError> {
        let rows = sqlx::query(
            "SELECT \
                approvals.id AS approval_id, \
                admission_decisions.id AS decision_id, \
                approvals.runtime_uid, \
                approvals.decision_key, \
                approvals.evidence_url, \
                admission_decisions.deltas, \
                admission_decisions.proposed_spec, \
                admission_decisions.base_spec_digest, \
                admission_decisions.base_pending_approval_digest, \
                admission_decisions.envelope_rev, \
                admission_decisions.actor, \
                admission_decisions.member_role \
             FROM approvals \
             JOIN admission_decisions \
               ON admission_decisions.id = approvals.admission_decision_id \
             WHERE approvals.state = 'pending' \
             ORDER BY admission_decisions.at, approvals.id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(database_error)?;
        rows.into_iter()
            .map(|row| {
                let Json(deltas) = row
                    .try_get::<Json<Vec<AdmissionDelta>>, _>("deltas")
                    .map_err(database_error)?;
                let Json(proposed_spec) = row
                    .try_get::<Json<AgentRuntimeSpec>, _>("proposed_spec")
                    .map_err(database_error)?;
                Ok(PendingApproval {
                    approval_id: row.try_get("approval_id").map_err(database_error)?,
                    decision_id: row.try_get("decision_id").map_err(database_error)?,
                    runtime_uid: row.try_get("runtime_uid").map_err(database_error)?,
                    decision_key: row.try_get("decision_key").map_err(database_error)?,
                    evidence_url: row.try_get("evidence_url").map_err(database_error)?,
                    deltas,
                    proposed_spec,
                    base_spec_digest: row.try_get("base_spec_digest").map_err(database_error)?,
                    base_pending_approval_digest: row
                        .try_get("base_pending_approval_digest")
                        .map_err(database_error)?,
                    envelope_revision: row.try_get("envelope_rev").map_err(database_error)?,
                    actor: row.try_get("actor").map_err(database_error)?,
                    member_role: row.try_get("member_role").map_err(database_error)?,
                })
            })
            .collect()
    }

    /// Resolve the only valid transitions for a parked create that encounters
    /// an approved winner:
    ///
    /// - inactive winner: leave the loser pending;
    /// - active winner, no filing lease: reject the loser;
    /// - active winner, filing lease: reject the loser but preserve the lease
    ///   so its external record can complete or be reclaimed;
    /// - terminal loser: return the active winner idempotently.
    pub async fn retire_pending_approval_if_superseded(
        &self,
        approval_id: Uuid,
        winning_approval_id: Uuid,
        runtime_uid: &str,
        decided_by: &str,
        rationale: &str,
    ) -> Result<Option<GrantReversion>, StoreError> {
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let rows = sqlx::query(
            "SELECT \
                approvals.id, \
                approvals.runtime_uid, \
                approvals.state, \
                admission_decisions.base_spec, \
                admission_decisions.proposed_spec, \
                admission_decisions.actor, \
                admission_decisions.member_role, \
                admission_decisions.runtime_namespace, \
                admission_decisions.runtime_name, \
                admission_decisions.base_pending_approval_digest, \
                admission_decisions.envelope_rev \
             FROM approvals \
             JOIN admission_decisions \
               ON admission_decisions.id = approvals.admission_decision_id \
             WHERE approvals.id = $1 OR approvals.id = $2 \
             ORDER BY approvals.id \
             FOR UPDATE OF approvals",
        )
        .bind(approval_id)
        .bind(winning_approval_id)
        .fetch_all(&mut *transaction)
        .await
        .map_err(database_error)?;
        let row_ids = rows
            .iter()
            .map(|row| row.try_get::<Uuid, _>("id").map_err(database_error))
            .collect::<Result<Vec<_>, _>>()?;
        let losing = row_ids
            .iter()
            .position(|id| *id == approval_id)
            .map(|index| &rows[index])
            .ok_or(StoreError::ApprovalNotFound)?;
        let winner = row_ids
            .iter()
            .position(|id| *id == winning_approval_id)
            .map(|index| &rows[index])
            .ok_or(StoreError::ApprovalNotFound)?;
        let losing_runtime_uid = losing
            .try_get::<String, _>("runtime_uid")
            .map_err(database_error)?;
        let winner_runtime_uid = winner
            .try_get::<String, _>("runtime_uid")
            .map_err(database_error)?;
        if losing_runtime_uid != runtime_uid || winner_runtime_uid != runtime_uid {
            return Err(StoreError::ApprovalNotFound);
        }
        let winner_state = winner
            .try_get::<String, _>("state")
            .map_err(database_error)?;
        if winner_state != "approved" {
            transaction.commit().await.map_err(database_error)?;
            return Ok(None);
        }
        let member_role = winner
            .try_get::<String, _>("member_role")
            .map_err(database_error)?;
        let envelope_revision = winner
            .try_get::<i64, _>("envelope_rev")
            .map_err(database_error)?;
        let runtime_namespace = winner
            .try_get::<String, _>("runtime_namespace")
            .map_err(database_error)?;
        let runtime_name = winner
            .try_get::<String, _>("runtime_name")
            .map_err(database_error)?;
        let actor = winner
            .try_get::<String, _>("actor")
            .map_err(database_error)?;
        let Json(base_spec) = winner
            .try_get::<Json<AgentRuntimeSpec>, _>("base_spec")
            .map_err(database_error)?;
        let Json(proposed_spec) = winner
            .try_get::<Json<AgentRuntimeSpec>, _>("proposed_spec")
            .map_err(database_error)?;
        let base_pending_approval_digest = winner
            .try_get("base_pending_approval_digest")
            .map_err(database_error)?;
        let losing_state = losing
            .try_get::<String, _>("state")
            .map_err(database_error)?;
        let scope_kind = envelope_scope_kind(&proposed_spec);
        lock_envelope_scope(&mut transaction, scope_kind, &member_role).await?;
        let grants = sqlx::query(
            "SELECT id \
             FROM grants \
             WHERE approval_id = $1 \
             ORDER BY id \
             FOR UPDATE",
        )
        .bind(winning_approval_id)
        .fetch_all(&mut *transaction)
        .await
        .map_err(database_error)?;
        let grants_active = if grants.is_empty() {
            false
        } else {
            sqlx::query_scalar::<_, bool>(
                "SELECT COALESCE(bool_and( \
                    grants.expires_at > clock_timestamp() \
                    AND grant_revocations.grant_id IS NULL \
                 ), false) \
                 FROM grants \
                 LEFT JOIN grant_revocations \
                   ON grant_revocations.grant_id = grants.id \
                 WHERE grants.approval_id = $1",
            )
            .bind(winning_approval_id)
            .fetch_one(&mut *transaction)
            .await
            .map_err(database_error)?
        };
        let latest_envelope_revision = sqlx::query_scalar::<_, i64>(
            "SELECT revision \
             FROM envelopes \
             WHERE scope_kind = $2 AND scope_ref = $1 \
             ORDER BY revision DESC \
             LIMIT 1",
        )
        .bind(&member_role)
        .bind(scope_kind.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?;
        if !grants_active || latest_envelope_revision != Some(envelope_revision) {
            transaction.commit().await.map_err(database_error)?;
            return Ok(None);
        }
        if losing_state == "pending" {
            let updated = sqlx::query(
                "UPDATE approvals \
                 SET state = 'rejected', \
                     decided_by = $2, \
                     decided_at = now(), \
                     rationale = $3 \
                 WHERE id = $1 \
                   AND state = 'pending'",
            )
            .bind(approval_id)
            .bind(decided_by)
            .bind(rationale)
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
            if updated.rows_affected() != 1 {
                return Err(StoreError::ApprovalNotPending);
            }
        }
        transaction.commit().await.map_err(database_error)?;
        Ok(Some(GrantReversion {
            runtime_uid: runtime_uid.to_owned(),
            runtime_namespace,
            runtime_name,
            actor,
            member_role,
            base_spec,
            proposed_spec,
            base_pending_approval_digest,
        }))
    }

    pub async fn link_decision_reference(
        &self,
        approval_id: Uuid,
        decision_key: &str,
        evidence_url: &str,
    ) -> Result<(), StoreError> {
        let updated = sqlx::query(
            "UPDATE approvals \
             SET decision_key = $1, evidence_url = $2 \
             WHERE id = $3 \
               AND state = 'pending' \
               AND decision_key IS NULL \
               AND evidence_url IS NULL",
        )
        .bind(decision_key)
        .bind(evidence_url)
        .bind(approval_id)
        .execute(&self.pool)
        .await
        .map_err(database_error)?;
        if updated.rows_affected() == 1 {
            return Ok(());
        }
        let row = sqlx::query(
            "SELECT state, decision_key, evidence_url \
             FROM approvals \
             WHERE id = $1",
        )
        .bind(approval_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(database_error)?
        .ok_or(StoreError::ApprovalNotFound)?;
        let state = row.try_get::<String, _>("state").map_err(database_error)?;
        let existing_key = row
            .try_get::<Option<String>, _>("decision_key")
            .map_err(database_error)?;
        let existing_url = row
            .try_get::<Option<String>, _>("evidence_url")
            .map_err(database_error)?;
        if existing_key.as_deref() == Some(decision_key)
            && existing_url.as_deref() == Some(evidence_url)
        {
            Ok(())
        } else if state != "pending" {
            Err(StoreError::ApprovalNotPending)
        } else {
            Err(StoreError::DecisionReferenceMismatch)
        }
    }

    pub async fn grants_for_runtime(
        &self,
        runtime_uid: &str,
        member_role: &str,
        envelope_revision: i64,
    ) -> Result<Vec<AdmissionDelta>, StoreError> {
        self.grants_for_runtime_scoped(
            runtime_uid,
            EnvelopeScopeKind::MemberRole,
            member_role,
            envelope_revision,
        )
        .await
    }

    pub async fn grants_for_runtime_scoped(
        &self,
        runtime_uid: &str,
        scope_kind: EnvelopeScopeKind,
        scope_ref: &str,
        envelope_revision: i64,
    ) -> Result<Vec<AdmissionDelta>, StoreError> {
        let rows = sqlx::query(
            "SELECT grants.granted_value \
             FROM grants \
             JOIN approvals ON approvals.id = grants.approval_id \
             JOIN admission_decisions \
               ON admission_decisions.id = approvals.admission_decision_id \
             LEFT JOIN grant_revocations ON grant_revocations.grant_id = grants.id \
             WHERE grants.runtime_uid = $1 \
               AND admission_decisions.member_role = $2 \
               AND admission_decisions.proposed_spec->'principal'->>'kind' = $3 \
               AND admission_decisions.envelope_rev = $4 \
               AND grants.envelope_revision = admission_decisions.envelope_rev \
               AND grants.expires_at > now() \
               AND grant_revocations.grant_id IS NULL \
             ORDER BY CASE grants.dimension \
                 WHEN 'budget' THEN 1 \
                 WHEN 'ttl' THEN 2 \
                 WHEN 'models' THEN 3 \
                 WHEN 'tools' THEN 4 \
                 ELSE 5 \
             END, grants.at, grants.id",
        )
        .bind(runtime_uid)
        .bind(scope_ref)
        .bind(match scope_kind {
            EnvelopeScopeKind::MemberRole => "user",
            EnvelopeScopeKind::Service => "service",
        })
        .bind(envelope_revision)
        .fetch_all(&self.pool)
        .await
        .map_err(database_error)?;
        rows.into_iter()
            .map(|row| {
                row.try_get::<Json<AdmissionDelta>, _>("granted_value")
                    .map(|Json(delta)| delta)
                    .map_err(database_error)
            })
            .collect()
    }

    pub async fn approval_candidate(
        &self,
        approval_id: Uuid,
        evidence_url: &str,
    ) -> Result<ApprovalCandidate, StoreError> {
        let row = sqlx::query(
            "SELECT \
                approvals.state, \
                approvals.evidence_url, \
                approvals.runtime_uid, \
                admission_decisions.proposed_spec, \
                admission_decisions.base_spec_digest, \
                admission_decisions.base_pending_approval_digest, \
                admission_decisions.actor, \
                admission_decisions.member_role, \
                admission_decisions.envelope_rev, \
                admission_decisions.runtime_namespace, \
                admission_decisions.runtime_name \
             FROM approvals \
             JOIN admission_decisions \
               ON admission_decisions.id = approvals.admission_decision_id \
             WHERE approvals.id = $1",
        )
        .bind(approval_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(database_error)?
        .ok_or(StoreError::ApprovalNotFound)?;
        let state = row.try_get::<String, _>("state").map_err(database_error)?;
        if state != "pending" && state != "approved" {
            return Err(StoreError::ApprovalNotPending);
        }
        let stored_evidence = row
            .try_get::<Option<String>, _>("evidence_url")
            .map_err(database_error)?
            .ok_or(StoreError::MissingDecisionReference)?;
        if stored_evidence != evidence_url {
            return Err(StoreError::EvidenceMismatch);
        }
        let Json(proposed_spec) = row
            .try_get::<Json<AgentRuntimeSpec>, _>("proposed_spec")
            .map_err(database_error)?;
        Ok(ApprovalCandidate {
            approval_id,
            runtime_uid: row.try_get("runtime_uid").map_err(database_error)?,
            proposed_spec,
            base_spec_digest: row.try_get("base_spec_digest").map_err(database_error)?,
            base_pending_approval_digest: row
                .try_get("base_pending_approval_digest")
                .map_err(database_error)?,
            actor: row.try_get("actor").map_err(database_error)?,
            member_role: row.try_get("member_role").map_err(database_error)?,
            envelope_revision: row.try_get("envelope_rev").map_err(database_error)?,
            runtime_namespace: row.try_get("runtime_namespace").map_err(database_error)?,
            runtime_name: row.try_get("runtime_name").map_err(database_error)?,
        })
    }

    pub async fn approval_for_filing(
        &self,
        approval_id: Uuid,
    ) -> Result<DecisionFiling, StoreError> {
        let row = sqlx::query(
            "SELECT \
                approvals.state, \
                approvals.decision_filing_token, \
                approvals.decision_key, \
                approvals.evidence_url, \
                approvals.runtime_uid, \
                admission_decisions.actor, \
                admission_decisions.member_role, \
                admission_decisions.deltas \
             FROM approvals \
             JOIN admission_decisions \
               ON admission_decisions.id = approvals.admission_decision_id \
             WHERE approvals.id = $1",
        )
        .bind(approval_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(database_error)?
        .ok_or(StoreError::ApprovalNotFound)?;
        let state = row.try_get::<String, _>("state").map_err(database_error)?;
        let filing_token = row
            .try_get::<Option<Uuid>, _>("decision_filing_token")
            .map_err(database_error)?;
        if state != "pending" && !(state == "rejected" && filing_token.is_some()) {
            return Err(StoreError::ApprovalNotPending);
        }
        let Json(deltas) = row
            .try_get::<Json<Vec<AdmissionDelta>>, _>("deltas")
            .map_err(database_error)?;
        Ok(DecisionFiling {
            approval_id,
            runtime_uid: row.try_get("runtime_uid").map_err(database_error)?,
            actor: row.try_get("actor").map_err(database_error)?,
            member_role: row.try_get("member_role").map_err(database_error)?,
            deltas,
            decision_key: row.try_get("decision_key").map_err(database_error)?,
            evidence_url: row.try_get("evidence_url").map_err(database_error)?,
        })
    }

    pub async fn claim_decision_filing(
        &self,
        approval_id: Uuid,
    ) -> Result<DecisionFilingClaim, StoreError> {
        let token = Uuid::new_v4();
        let row = sqlx::query(
            "UPDATE approvals \
             SET decision_filing_token = $2, decision_filing_started_at = now() \
             WHERE id = $1 \
               AND ( \
                    state = 'pending' \
                    OR (state = 'rejected' AND decision_filing_token IS NOT NULL) \
               ) \
               AND decision_key IS NULL \
               AND evidence_url IS NULL \
               AND (decision_filing_token IS NULL \
                    OR decision_filing_started_at < now() - interval '5 minutes') \
             RETURNING id",
        )
        .bind(approval_id)
        .bind(token)
        .fetch_optional(&self.pool)
        .await
        .map_err(database_error)?;
        if row.is_some() {
            return Ok(DecisionFilingClaim {
                filing: self.approval_for_filing(approval_id).await?,
                token: Some(token),
            });
        }
        let filing = self.approval_for_filing(approval_id).await?;
        if filing.decision_key.is_some() && filing.evidence_url.is_some() {
            Ok(DecisionFilingClaim {
                filing,
                token: None,
            })
        } else {
            Err(StoreError::DecisionFilingInProgress)
        }
    }

    pub async fn complete_decision_filing(
        &self,
        approval_id: Uuid,
        token: Uuid,
        decision_key: &str,
        evidence_url: &str,
    ) -> Result<(), StoreError> {
        let updated = sqlx::query(
            "UPDATE approvals \
             SET decision_key = $3, evidence_url = $4, \
                 decision_filing_token = NULL, decision_filing_started_at = NULL \
             WHERE id = $1 \
               AND decision_filing_token = $2 \
               AND state IN ('pending', 'rejected') \
               AND decision_key IS NULL \
               AND evidence_url IS NULL",
        )
        .bind(approval_id)
        .bind(token)
        .bind(decision_key)
        .bind(evidence_url)
        .execute(&self.pool)
        .await
        .map_err(database_error)?;
        if updated.rows_affected() == 1 {
            Ok(())
        } else {
            Err(StoreError::DecisionFilingClaimLost)
        }
    }

    pub async fn release_decision_filing(
        &self,
        approval_id: Uuid,
        token: Uuid,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE approvals \
             SET decision_filing_token = NULL, decision_filing_started_at = NULL \
             WHERE id = $1 AND decision_filing_token = $2",
        )
        .bind(approval_id)
        .bind(token)
        .execute(&self.pool)
        .await
        .map_err(database_error)?;
        Ok(())
    }

    pub async fn grant_reversion(
        &self,
        runtime_uid: &str,
    ) -> Result<Option<GrantReversion>, StoreError> {
        let row = sqlx::query(
            "SELECT \
                admission_decisions.base_spec, \
                admission_decisions.proposed_spec, \
                admission_decisions.actor, \
                admission_decisions.member_role, \
                admission_decisions.runtime_namespace, \
                admission_decisions.runtime_name, \
                admission_decisions.base_pending_approval_digest, \
                admission_decisions.envelope_rev, \
                latest_envelope.revision AS latest_envelope_rev, \
                bool_and( \
                    grants.expires_at > now() \
                    AND grant_revocations.grant_id IS NULL \
                ) AS grants_active \
             FROM approvals \
             JOIN admission_decisions \
               ON admission_decisions.id = approvals.admission_decision_id \
             JOIN grants ON grants.approval_id = approvals.id \
             LEFT JOIN grant_revocations ON grant_revocations.grant_id = grants.id \
             LEFT JOIN LATERAL ( \
                 SELECT revision \
                 FROM envelopes \
                 WHERE scope_kind = CASE \
                       WHEN admission_decisions.proposed_spec->'principal'->>'kind' = 'service' \
                       THEN 'service' ELSE 'member_role' END \
                   AND scope_ref = admission_decisions.member_role \
                 ORDER BY revision DESC \
                 LIMIT 1 \
             ) latest_envelope ON true \
             WHERE approvals.runtime_uid = $1 \
               AND approvals.state = 'approved' \
             GROUP BY \
                approvals.decided_at, approvals.id, admission_decisions.id, \
                latest_envelope.revision \
             ORDER BY approvals.decided_at DESC, approvals.id DESC \
             LIMIT 1",
        )
        .bind(runtime_uid)
        .fetch_optional(&self.pool)
        .await
        .map_err(database_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let envelope_revision: i64 = row.try_get("envelope_rev").map_err(database_error)?;
        let latest_envelope_revision: Option<i64> =
            row.try_get("latest_envelope_rev").map_err(database_error)?;
        let grants_active: bool = row.try_get("grants_active").map_err(database_error)?;
        if grants_active && latest_envelope_revision == Some(envelope_revision) {
            return Ok(None);
        }
        let Json(base_spec) = row
            .try_get::<Json<AgentRuntimeSpec>, _>("base_spec")
            .map_err(database_error)?;
        let Json(proposed_spec) = row
            .try_get::<Json<AgentRuntimeSpec>, _>("proposed_spec")
            .map_err(database_error)?;
        Ok(Some(GrantReversion {
            runtime_uid: runtime_uid.to_owned(),
            runtime_namespace: row.try_get("runtime_namespace").map_err(database_error)?,
            runtime_name: row.try_get("runtime_name").map_err(database_error)?,
            actor: row.try_get("actor").map_err(database_error)?,
            member_role: row.try_get("member_role").map_err(database_error)?,
            base_spec,
            proposed_spec,
            base_pending_approval_digest: row
                .try_get("base_pending_approval_digest")
                .map_err(database_error)?,
        }))
    }

    pub async fn grant_application(
        &self,
        runtime_uid: &str,
    ) -> Result<Option<GrantApplication>, StoreError> {
        let row = sqlx::query(
            "SELECT \
                approvals.id AS approval_id, \
                admission_decisions.base_spec, \
                admission_decisions.proposed_spec, \
                admission_decisions.actor, \
                admission_decisions.member_role, \
                admission_decisions.runtime_namespace, \
                admission_decisions.runtime_name, \
                admission_decisions.base_pending_approval_digest, \
                admission_decisions.envelope_rev, \
                latest_envelope.revision AS latest_envelope_rev, \
                bool_and( \
                    grants.expires_at > clock_timestamp() \
                    AND grant_revocations.grant_id IS NULL \
                ) AS grants_active \
             FROM approvals \
             JOIN admission_decisions \
               ON admission_decisions.id = approvals.admission_decision_id \
             JOIN grants ON grants.approval_id = approvals.id \
             LEFT JOIN grant_revocations ON grant_revocations.grant_id = grants.id \
             JOIN LATERAL ( \
                 SELECT revision \
                 FROM envelopes \
                 WHERE scope_kind = CASE \
                       WHEN admission_decisions.proposed_spec->'principal'->>'kind' = 'service' \
                       THEN 'service' ELSE 'member_role' END \
                   AND scope_ref = admission_decisions.member_role \
                 ORDER BY revision DESC \
                 LIMIT 1 \
             ) latest_envelope ON true \
             WHERE approvals.runtime_uid = $1 \
               AND approvals.state = 'approved' \
             GROUP BY \
                approvals.decided_at, approvals.id, admission_decisions.id, \
                latest_envelope.revision \
             ORDER BY approvals.decided_at DESC, approvals.id DESC \
             LIMIT 1",
        )
        .bind(runtime_uid)
        .fetch_optional(&self.pool)
        .await
        .map_err(database_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let envelope_revision: i64 = row.try_get("envelope_rev").map_err(database_error)?;
        let latest_envelope_revision: Option<i64> =
            row.try_get("latest_envelope_rev").map_err(database_error)?;
        let grants_active: bool = row.try_get("grants_active").map_err(database_error)?;
        if !grants_active || latest_envelope_revision != Some(envelope_revision) {
            return Ok(None);
        }
        let Json(base_spec) = row
            .try_get::<Json<AgentRuntimeSpec>, _>("base_spec")
            .map_err(database_error)?;
        let Json(proposed_spec) = row
            .try_get::<Json<AgentRuntimeSpec>, _>("proposed_spec")
            .map_err(database_error)?;
        Ok(Some(GrantApplication {
            approval_id: row.try_get("approval_id").map_err(database_error)?,
            application: GrantReversion {
                runtime_uid: runtime_uid.to_owned(),
                runtime_namespace: row.try_get("runtime_namespace").map_err(database_error)?,
                runtime_name: row.try_get("runtime_name").map_err(database_error)?,
                actor: row.try_get("actor").map_err(database_error)?,
                member_role: row.try_get("member_role").map_err(database_error)?,
                base_spec,
                proposed_spec,
                base_pending_approval_digest: row
                    .try_get("base_pending_approval_digest")
                    .map_err(database_error)?,
            },
        }))
    }

    pub async fn approve_admission(
        &self,
        request: ApproveAdmission<'_>,
    ) -> Result<ApprovedAdmission, StoreError> {
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let row = sqlx::query(
            "SELECT \
                approvals.state, \
                approvals.decision_key, \
                approvals.evidence_url, \
                approvals.decided_by, \
                approvals.rationale, \
                approvals.admission_decision_id, \
                approvals.runtime_uid, \
                admission_decisions.deltas, \
                admission_decisions.proposed_spec, \
                admission_decisions.base_spec_digest, \
                admission_decisions.envelope_rev, \
                admission_decisions.actor, \
                admission_decisions.member_role \
             FROM approvals \
             JOIN admission_decisions \
               ON admission_decisions.id = approvals.admission_decision_id \
             WHERE approvals.id = $1 \
             FOR UPDATE OF approvals",
        )
        .bind(request.approval_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?
        .ok_or(StoreError::ApprovalNotFound)?;
        let state = row.try_get::<String, _>("state").map_err(database_error)?;
        let decision_key = row
            .try_get::<Option<String>, _>("decision_key")
            .map_err(database_error)?;
        let evidence_url = row
            .try_get::<Option<String>, _>("evidence_url")
            .map_err(database_error)?;
        let (Some(decision_key), Some(evidence_url)) = (decision_key, evidence_url) else {
            return Err(StoreError::MissingDecisionReference);
        };
        if evidence_url != request.evidence_url {
            return Err(StoreError::EvidenceMismatch);
        }
        let decision_id = row
            .try_get("admission_decision_id")
            .map_err(database_error)?;
        let runtime_uid = row.try_get("runtime_uid").map_err(database_error)?;
        let Json(grants) = row
            .try_get::<Json<Vec<AdmissionDelta>>, _>("deltas")
            .map_err(database_error)?;
        let Json(proposed_spec) = row
            .try_get::<Json<AgentRuntimeSpec>, _>("proposed_spec")
            .map_err(database_error)?;
        let base_spec_digest = row.try_get("base_spec_digest").map_err(database_error)?;
        let envelope_revision: i64 = row.try_get("envelope_rev").map_err(database_error)?;
        let actor = row.try_get("actor").map_err(database_error)?;
        let member_role: String = row.try_get("member_role").map_err(database_error)?;
        if state == "approved" {
            let decided_by = row
                .try_get::<Option<String>, _>("decided_by")
                .map_err(database_error)?
                .ok_or(StoreError::ApprovalNotPending)?;
            let rationale = row
                .try_get::<Option<String>, _>("rationale")
                .map_err(database_error)?
                .ok_or(StoreError::ApprovalNotPending)?;
            return Ok(ApprovedAdmission {
                approval_id: request.approval_id,
                decision_id,
                runtime_uid,
                proposed_spec,
                base_spec_digest,
                actor,
                member_role,
                decision_key,
                evidence_url,
                grants,
                decided_by,
                rationale,
            });
        }
        if state != "pending" {
            return Err(StoreError::ApprovalNotPending);
        }
        let scope_kind = envelope_scope_kind(&proposed_spec);
        lock_envelope_scope(&mut transaction, scope_kind, &member_role).await?;
        let latest_revision = sqlx::query_scalar::<_, i64>(
            "SELECT revision \
             FROM envelopes \
             WHERE scope_kind = $2 AND scope_ref = $1 \
             ORDER BY revision DESC \
             LIMIT 1",
        )
        .bind(&member_role)
        .bind(scope_kind.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?;
        if latest_revision != Some(envelope_revision) {
            return Err(StoreError::StaleEnvelope);
        }

        let updated = sqlx::query(
            "UPDATE approvals \
             SET state = 'approved', \
                 decided_by = $1, \
                 decided_at = now(), \
                 rationale = $2 \
             WHERE id = $3 \
               AND ($4::text)::timestamptz > now()",
        )
        .bind(request.decided_by)
        .bind(request.rationale)
        .bind(request.approval_id)
        .bind(request.expires_at)
        .execute(&mut *transaction)
        .await
        .map_err(grant_expiry_error)?;
        if updated.rows_affected() != 1 {
            return Err(StoreError::InvalidGrantExpiry);
        }
        for grant in &grants {
            sqlx::query(
                "INSERT INTO grants \
                 (id, runtime_uid, dimension, granted_value, approval_id, \
                  envelope_revision, expires_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, ($7::text)::timestamptz)",
            )
            .bind(Uuid::new_v4())
            .bind(&runtime_uid)
            .bind(grant_dimension(grant))
            .bind(Json(grant))
            .bind(request.approval_id)
            .bind(envelope_revision)
            .bind(request.expires_at)
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
        }
        transaction.commit().await.map_err(database_error)?;
        Ok(ApprovedAdmission {
            approval_id: request.approval_id,
            decision_id,
            runtime_uid,
            proposed_spec,
            base_spec_digest,
            actor,
            member_role,
            decision_key,
            evidence_url,
            grants,
            decided_by: request.decided_by.to_owned(),
            rationale: request.rationale.to_owned(),
        })
    }

    pub async fn revoke_runtime_grants(
        &self,
        runtime_uid: &str,
        revoked_by: &str,
        reason: &str,
    ) -> Result<u64, StoreError> {
        if reason.is_empty() {
            return Err(StoreError::MissingRevocationReason);
        }
        let result = sqlx::query(
            "INSERT INTO grant_revocations (grant_id, revoked_by, reason) \
             SELECT grants.id, $2, $3 \
             FROM grants \
             LEFT JOIN grant_revocations ON grant_revocations.grant_id = grants.id \
             WHERE grants.runtime_uid = $1 \
               AND grants.expires_at > now() \
               AND grant_revocations.grant_id IS NULL \
             ON CONFLICT (grant_id) DO NOTHING",
        )
        .bind(runtime_uid)
        .bind(revoked_by)
        .bind(reason)
        .execute(&self.pool)
        .await
        .map_err(database_error)?;
        Ok(result.rows_affected())
    }
}

impl PgStore {
    pub async fn reserve_task(
        &self,
        request: &TaskReservationRequest<'_>,
    ) -> Result<TaskReservation, StoreError> {
        validate_task_identity_binding(request)?;
        validate_task_version_pins(request)?;
        let task_uid = Uuid::new_v4();
        let inserted = sqlx::query(
            "INSERT INTO task_submissions \
             (task_uid, idempotency_key, submitter_service, acting_user, acting_user_id, \
              owner, owner_user_id, identity_binding_state, workflow, \
              workflow_name, workflow_version, workflow_digest, \
              user_envelope_instance_id, user_envelope_revision, user_envelope_digest, \
              coding_agent_runtime, runtime_namespace, runtime_name, runtime_ownership, phase, \
              runtime_spec, agent_command, execution_binding, envelope_revision) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'bound', $8, $9, $10, $11, $12, $13, $14, \
                     $15, $16, $17, $18, 'submitted', $19, $20, $21, $22) \
             ON CONFLICT DO NOTHING",
        )
        .bind(task_uid)
        .bind(request.idempotency_key)
        .bind(request.submitter_service)
        .bind(request.acting_user)
        .bind(request.acting_user_id)
        .bind(request.owner)
        .bind(request.owner_user_id)
        .bind(request.workflow)
        .bind(request.workflow_name)
        .bind(request.workflow_version)
        .bind(request.workflow_digest)
        .bind(request.user_envelope_instance_id)
        .bind(request.user_envelope_revision)
        .bind(request.user_envelope_digest)
        .bind(request.coding_agent_runtime)
        .bind(request.runtime_namespace)
        .bind(request.runtime_name)
        .bind(ownership_text(request.runtime_ownership))
        .bind(Json(request.runtime_spec))
        .bind(Json(request.agent_command))
        .bind(request.execution_binding.map(Json))
        .bind(request.envelope_revision)
        .execute(&self.pool)
        .await
        .map_err(database_error)?
        .rows_affected()
            == 1;
        let record = self
            .task_by_idempotency(
                request.submitter_service,
                request.owner_user_id,
                request.idempotency_key,
            )
            .await?
            .ok_or_else(|| {
                StoreError::Database(
                    "task reservation disappeared after idempotent insert".to_owned(),
                )
            })?;
        if record.submitter_service != request.submitter_service
            || record.acting_user.as_deref() != request.acting_user
            || record.acting_user_id.as_deref() != request.acting_user_id
            || record.owner != request.owner
            || record.owner_user_id.as_deref() != Some(request.owner_user_id)
            || record.workflow != request.workflow
            || record.workflow_name.as_deref() != request.workflow_name
            || record.workflow_version != request.workflow_version
            || record.workflow_digest.as_deref() != request.workflow_digest
            || record.user_envelope_instance_id.as_deref() != request.user_envelope_instance_id
            || record.user_envelope_revision != request.user_envelope_revision
            || record.user_envelope_digest.as_deref() != request.user_envelope_digest
            || record.coding_agent_runtime != request.coding_agent_runtime
            || record.runtime_namespace != request.runtime_namespace
            || record.runtime_name != request.runtime_name
            || record.runtime_ownership != request.runtime_ownership
            || record.runtime_spec != *request.runtime_spec
            || record.agent_command != request.agent_command
            || record.execution_binding.as_ref() != request.execution_binding
        {
            return Err(StoreError::TaskIdempotencyConflict);
        }
        Ok(TaskReservation { inserted, record })
    }

    /// Atomically reserves one internal provider-control task and its dedicated projection.
    /// Durable advisory locking makes coalescing and mutation serialization work across
    /// apiserver replicas and process restarts.
    pub async fn reserve_connection_operation(
        &self,
        request: &ConnectionOperationReservationRequest<'_>,
    ) -> Result<ConnectionOperationReservation, StoreError> {
        validate_connection_operation_request(request)?;
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("connection:{}:github", request.task.owner_user_id))
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;

        sqlx::query(
            "WITH drifted AS ( \
               UPDATE connection_operations \
               SET operation_state = 'failed', failure_category = 'binding_mismatch', \
                   result = NULL, cached_status = NULL, cache_expires_at = NULL, \
                   result_expires_at = NULL, finalization_state = 'requested', \
                   cleanup_state = 'tearing_down', updated_at = now() \
               WHERE canonical_user_id = $1 AND provider = 'github' \
                 AND operation_state IN ('queued', 'provisioning', 'running') \
                 AND finalization_state = 'not_requested' \
                 AND NOT (artifact_trust_mode = $2 AND bridge_image_digest = $3 \
                   AND mcp_gw_origin = $4 AND mcp_gw_version = $5 \
                   AND runtime_namespace = $6 AND runtime_class = $7) \
               RETURNING task_uid \
             ) \
             UPDATE task_submissions \
             SET phase = CASE WHEN phase IN ('succeeded', 'failed') THEN phase ELSE 'failed' END, \
                 output_archive = NULL, finalize_requested = true, \
                 failure_reason = 'binding_mismatch', updated_at = now() \
             WHERE task_uid IN (SELECT task_uid FROM drifted)",
        )
        .bind(request.task.owner_user_id)
        .bind(&request.bindings.artifact_trust_mode)
        .bind(&request.bindings.bridge_image_digest)
        .bind(&request.bindings.mcp_gw_origin)
        .bind(&request.bindings.mcp_gw_version)
        .bind(&request.bindings.namespace)
        .bind(&request.bindings.runtime_class)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;

        sqlx::query(
            "UPDATE connection_operations \
             SET cached_status = NULL, cache_expires_at = NULL, result_expires_at = NULL, \
                 updated_at = now() \
             WHERE canonical_user_id = $1 AND provider = 'github' \
               AND operation_state = 'succeeded' \
               AND NOT (artifact_trust_mode = $2 AND bridge_image_digest = $3 \
                 AND mcp_gw_origin = $4 AND mcp_gw_version = $5 \
                 AND runtime_namespace = $6 AND runtime_class = $7)",
        )
        .bind(request.task.owner_user_id)
        .bind(&request.bindings.artifact_trust_mode)
        .bind(&request.bindings.bridge_image_digest)
        .bind(&request.bindings.mcp_gw_origin)
        .bind(&request.bindings.mcp_gw_version)
        .bind(&request.bindings.namespace)
        .bind(&request.bindings.runtime_class)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;

        sqlx::query(
            "UPDATE connection_operations \
             SET oauth_phase = 'expired', authorization_url = NULL, updated_at = now() \
             WHERE canonical_user_id = $1 AND provider = 'github' \
               AND oauth_phase = 'pending' AND flow_expires_at <= now()",
        )
        .bind(request.task.owner_user_id)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;

        if request.operation_kind == ConnectionOperationKind::Start {
            let mismatched_pending_flow = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM connection_operations \
                 WHERE canonical_user_id = $1 AND provider = 'github' \
                   AND oauth_phase = 'pending' AND flow_expires_at > now() \
                   AND NOT (artifact_trust_mode = $2 AND bridge_image_digest = $3 \
                     AND mcp_gw_origin = $4 AND mcp_gw_version = $5 \
                     AND runtime_namespace = $6 AND runtime_class = $7))",
            )
            .bind(request.task.owner_user_id)
            .bind(&request.bindings.artifact_trust_mode)
            .bind(&request.bindings.bridge_image_digest)
            .bind(&request.bindings.mcp_gw_origin)
            .bind(&request.bindings.mcp_gw_version)
            .bind(&request.bindings.namespace)
            .bind(&request.bindings.runtime_class)
            .fetch_one(&mut *transaction)
            .await
            .map_err(database_error)?;
            if mismatched_pending_flow {
                transaction.commit().await.map_err(database_error)?;
                return Err(StoreError::ConnectionOAuthFlowPending);
            }
        }

        let active_mutation = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM connection_operations \
             WHERE canonical_user_id = $1 AND provider = 'github' \
               AND operation_kind IN ('start', 'disconnect') \
               AND operation_state IN ('queued', 'provisioning', 'running') \
               AND finalization_state = 'not_requested')",
        )
        .bind(request.task.owner_user_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(database_error)?;
        if active_mutation && request.operation_kind == ConnectionOperationKind::Status {
            return Err(StoreError::ConnectionOperationConflict);
        }

        let reusable = match request.operation_kind {
            ConnectionOperationKind::Status => sqlx::query(
                "SELECT operations.*, \
                        to_char(operations.flow_expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS flow_expires_at_text, \
                        to_char(operations.response_deadline_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS response_deadline_at_text, \
                        tasks.phase AS task_phase, tasks.runtime_uid, \
                        tasks.output_archive, tasks.finalize_requested, tasks.finalized \
                 FROM connection_operations operations \
                 JOIN task_submissions tasks ON tasks.task_uid = operations.task_uid \
                 WHERE operations.canonical_user_id = $1 AND operations.provider = 'github' \
                   AND operations.operation_kind = 'status' \
                   AND ( \
                     (operations.operation_state IN ('queued', 'provisioning', 'running') \
                        AND operations.finalization_state = 'not_requested') \
                     OR (operations.operation_state = 'succeeded' \
                        AND operations.cache_expires_at > now() AND $2 \
                        AND NOT (operations.cached_status->>'connected' = 'false' \
                          AND EXISTS (SELECT 1 FROM connection_operations pending \
                            WHERE pending.canonical_user_id = operations.canonical_user_id \
                              AND pending.provider = operations.provider \
                              AND pending.oauth_phase = 'pending' \
                              AND pending.flow_expires_at > now()))) \
                   ) \
                 ORDER BY operations.created_at DESC LIMIT 1",
            )
            .bind(request.task.owner_user_id)
            .bind(request.allow_status_cache)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(database_error)?,
            ConnectionOperationKind::Start => sqlx::query(
                "SELECT operations.*, \
                        to_char(operations.flow_expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS flow_expires_at_text, \
                        to_char(operations.response_deadline_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS response_deadline_at_text, \
                        tasks.phase AS task_phase, tasks.runtime_uid, \
                        tasks.output_archive, tasks.finalize_requested, tasks.finalized \
                 FROM connection_operations operations \
                 JOIN task_submissions tasks ON tasks.task_uid = operations.task_uid \
                 WHERE operations.canonical_user_id = $1 AND operations.provider = 'github' \
                   AND operations.operation_kind = 'start' \
                   AND (operations.oauth_phase = 'pending' AND operations.flow_expires_at > now() \
                     OR operations.operation_state IN ('queued', 'provisioning', 'running')) \
                 ORDER BY operations.created_at DESC LIMIT 1",
            )
            .bind(request.task.owner_user_id)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(database_error)?,
            ConnectionOperationKind::Disconnect => {
                let pending = sqlx::query_scalar::<_, Option<Uuid>>(
                    "SELECT operation_id FROM connection_operations \
                     WHERE canonical_user_id = $1 AND provider = 'github' \
                       AND oauth_phase = 'pending' AND flow_expires_at > now() \
                     ORDER BY created_at DESC LIMIT 1",
                )
                .bind(request.task.owner_user_id)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(database_error)?
                .flatten();
                if let Some(pending_operation_id) = pending {
                    let connected_after_start = sqlx::query_scalar::<_, bool>(
                        "SELECT EXISTS( \
                           SELECT 1 FROM connection_operations status \
                           JOIN connection_operations pending \
                             ON pending.operation_id = $2 \
                           WHERE status.canonical_user_id = $1 \
                             AND status.provider = 'github' \
                             AND status.operation_kind = 'status' \
                             AND status.uncached_status \
                             AND status.operation_state = 'succeeded' \
                             AND status.created_at >= pending.flow_created_at \
                             AND status.result->>'connected' = 'true')",
                    )
                    .bind(request.task.owner_user_id)
                    .bind(pending_operation_id)
                    .fetch_one(&mut *transaction)
                    .await
                    .map_err(database_error)?;
                    if !connected_after_start {
                        return Err(StoreError::ConnectionOAuthFlowPending);
                    }
                    sqlx::query(
                        "UPDATE connection_operations \
                         SET oauth_phase = 'completed', authorization_url = NULL, \
                             cache_expires_at = NULL, updated_at = now() \
                         WHERE operation_id = $1 AND oauth_phase = 'pending'",
                    )
                    .bind(pending_operation_id)
                    .execute(&mut *transaction)
                    .await
                    .map_err(database_error)?;
                }
                sqlx::query(
                    "SELECT operations.*, \
                            to_char(operations.flow_expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS flow_expires_at_text, \
                            to_char(operations.response_deadline_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS response_deadline_at_text, \
                            tasks.phase AS task_phase, tasks.runtime_uid, \
                            tasks.output_archive, tasks.finalize_requested, tasks.finalized \
                     FROM connection_operations operations \
                     JOIN task_submissions tasks ON tasks.task_uid = operations.task_uid \
                     WHERE operations.canonical_user_id = $1 AND operations.provider = 'github' \
                       AND operations.operation_kind = 'disconnect' \
                       AND (operations.operation_state IN ('queued', 'provisioning', 'running') \
                         OR (operations.operation_state = 'succeeded' \
                            AND operations.result_expires_at > now())) \
                     ORDER BY operations.created_at DESC LIMIT 1",
                )
                .bind(request.task.owner_user_id)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(database_error)?
            }
        };
        if let Some(row) = reusable {
            let record = connection_operation_record(row)?;
            if connection_execution_bindings_match(&record.bindings, request.bindings) {
                transaction.commit().await.map_err(database_error)?;
                return Ok(ConnectionOperationReservation {
                    inserted: false,
                    record,
                });
            }
        }

        if active_mutation {
            return Err(StoreError::ConnectionOperationConflict);
        }
        if request.operation_kind != ConnectionOperationKind::Status {
            sqlx::query(
                "WITH preempted AS ( \
                   UPDATE connection_operations \
                   SET operation_state = 'failed', failure_category = 'superseded_by_mutation', \
                       result = NULL, cached_status = NULL, cache_expires_at = NULL, \
                       finalization_state = 'requested', cleanup_state = 'tearing_down', \
                       updated_at = now() \
                   WHERE canonical_user_id = $1 AND provider = 'github' \
                     AND operation_kind = 'status' \
                     AND operation_state IN ('queued', 'provisioning', 'running') \
                     AND finalization_state = 'not_requested' \
                   RETURNING task_uid \
                 ) \
                 UPDATE task_submissions tasks \
                 SET phase = CASE WHEN tasks.phase IN ('succeeded', 'failed') \
                         THEN tasks.phase ELSE 'failed' END, \
                     output_archive = NULL, finalize_requested = true, \
                     failure_reason = 'superseded_by_mutation', updated_at = now() \
                 FROM preempted WHERE tasks.task_uid = preempted.task_uid",
            )
            .bind(request.task.owner_user_id)
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
            sqlx::query(
                "UPDATE connection_operations \
                 SET cache_expires_at = NULL, result_expires_at = NULL, updated_at = now() \
                 WHERE canonical_user_id = $1 AND provider = 'github'",
            )
            .bind(request.task.owner_user_id)
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
        }

        let task = &request.task;
        sqlx::query(
            "INSERT INTO task_submissions \
             (task_uid, idempotency_key, submitter_service, acting_user, acting_user_id, \
              owner, owner_user_id, identity_binding_state, workflow, coding_agent_runtime, \
              runtime_namespace, runtime_name, runtime_ownership, phase, runtime_spec, \
              agent_command, input_archive, execute_requested, envelope_revision, \
              internal_authority_id, internal_authority_version, internal_authority_digest) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'bound', $8, $9, $10, $11, \
                     'provisioned', 'queued', $12, $13, $14, true, $15, $16, $17, $18)",
        )
        .bind(request.operation_id)
        .bind(task.idempotency_key)
        .bind(task.submitter_service)
        .bind(task.acting_user)
        .bind(task.acting_user_id)
        .bind(task.owner)
        .bind(task.owner_user_id)
        .bind(task.workflow)
        .bind(task.coding_agent_runtime)
        .bind(task.runtime_namespace)
        .bind(task.runtime_name)
        .bind(Json(task.runtime_spec))
        .bind(Json(task.agent_command))
        .bind(request.input_archive)
        .bind(task.envelope_revision)
        .bind(request.authority_id)
        .bind(request.authority_version)
        .bind(request.authority_digest)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        let row = sqlx::query(
            "INSERT INTO connection_operations \
             (operation_id, task_uid, canonical_user_id, provider, operation_kind, \
              submitter_service, authority_id, authority_version, authority_digest, \
              runtime_spec_snapshot, command_snapshot, artifact_trust_mode, bridge_image_digest, mcp_gw_origin, \
              mcp_gw_version, runtime_namespace, runtime_class, idempotency_identity, uncached_status, \
              response_deadline_at) \
             VALUES ($1, $1, $2, 'github', $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, \
                     $13, $14, $15, $16, $17, now() + make_interval(secs => $18)) \
             RETURNING *, \
                       to_char(flow_expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS flow_expires_at_text, \
                       to_char(response_deadline_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS response_deadline_at_text, \
                       'queued' AS task_phase, NULL::text AS runtime_uid, \
                       NULL::bytea AS output_archive, false AS finalize_requested, \
                       false AS finalized",
        )
        .bind(request.operation_id)
        .bind(task.owner_user_id)
        .bind(request.operation_kind.as_str())
        .bind(task.submitter_service)
        .bind(request.authority_id)
        .bind(request.authority_version)
        .bind(request.authority_digest)
        .bind(Json(task.runtime_spec))
        .bind(Json(task.agent_command))
        .bind(&request.bindings.artifact_trust_mode)
        .bind(&request.bindings.bridge_image_digest)
        .bind(&request.bindings.mcp_gw_origin)
        .bind(&request.bindings.mcp_gw_version)
        .bind(&request.bindings.namespace)
        .bind(&request.bindings.runtime_class)
        .bind(request.idempotency_identity)
        .bind(
            request.operation_kind == ConnectionOperationKind::Status
                && !request.allow_status_cache,
        )
        .bind(request.response_deadline_seconds as f64)
        .fetch_one(&mut *transaction)
        .await
        .map_err(database_error)?;
        let record = connection_operation_record(row)?;
        transaction.commit().await.map_err(database_error)?;
        Ok(ConnectionOperationReservation {
            inserted: true,
            record,
        })
    }

    pub async fn connection_operation(
        &self,
        operation_id: Uuid,
        canonical_user_id: &CanonicalUserId,
    ) -> Result<Option<ConnectionOperationRecord>, StoreError> {
        sqlx::query(
            "SELECT operations.*, \
                    to_char(operations.flow_expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS flow_expires_at_text, \
                    to_char(operations.response_deadline_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS response_deadline_at_text, \
                    tasks.phase AS task_phase, tasks.runtime_uid, \
                    tasks.output_archive, tasks.finalize_requested, tasks.finalized \
             FROM connection_operations operations \
             JOIN task_submissions tasks ON tasks.task_uid = operations.task_uid \
             WHERE operations.operation_id = $1 AND operations.canonical_user_id = $2",
        )
        .bind(operation_id)
        .bind(canonical_user_id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(database_error)?
        .map(connection_operation_record)
            .transpose()
    }

    /// Internal controller lookup. Dedicated connection operations are never exposed through
    /// generic task or run read models, but the controller must recover their immutable binding
    /// snapshot before it provisions or executes the referenced task.
    pub async fn connection_operation_for_task(
        &self,
        task_uid: Uuid,
    ) -> Result<Option<ConnectionOperationRecord>, StoreError> {
        sqlx::query(
            "SELECT operations.*, \
                    to_char(operations.flow_expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS flow_expires_at_text, \
                    to_char(operations.response_deadline_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS response_deadline_at_text, \
                    tasks.phase AS task_phase, tasks.runtime_uid, \
                    tasks.output_archive, tasks.finalize_requested, tasks.finalized \
             FROM connection_operations operations \
             JOIN task_submissions tasks ON tasks.task_uid = operations.task_uid \
             WHERE operations.task_uid = $1",
        )
        .bind(task_uid)
        .fetch_optional(&self.pool)
        .await
        .map_err(database_error)?
        .map(connection_operation_record)
        .transpose()
    }

    /// Internal runtime-controller lookup. The runtime UID comes from Kubernetes and is matched
    /// through the dedicated task projection; caller-visible task/run queries remain unable to
    /// discover connection operations.
    pub async fn connection_operation_for_runtime(
        &self,
        runtime_uid: &str,
    ) -> Result<Option<ConnectionOperationRecord>, StoreError> {
        let rows = sqlx::query(
            "SELECT operations.*, \
                    to_char(operations.flow_expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS flow_expires_at_text, \
                    to_char(operations.response_deadline_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS response_deadline_at_text, \
                    tasks.phase AS task_phase, tasks.runtime_uid, \
                    tasks.output_archive, tasks.finalize_requested, tasks.finalized \
             FROM connection_operations operations \
             JOIN task_submissions tasks ON tasks.task_uid = operations.task_uid \
             WHERE tasks.runtime_uid = $1 \
             ORDER BY operations.created_at DESC LIMIT 2",
        )
        .bind(runtime_uid)
        .fetch_all(&self.pool)
        .await
        .map_err(database_error)?;
        if rows.len() > 1 {
            return Err(StoreError::InvalidConnectionOperation);
        }
        rows.into_iter()
            .next()
            .map(connection_operation_record)
            .transpose()
    }

    pub async fn connection_operations_requiring_reconcile(
        &self,
    ) -> Result<Vec<ConnectionOperationRecord>, StoreError> {
        sqlx::query(
            "SELECT operations.*, \
                    to_char(operations.flow_expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS flow_expires_at_text, \
                    to_char(operations.response_deadline_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS response_deadline_at_text, \
                    tasks.phase AS task_phase, tasks.runtime_uid, \
                    tasks.output_archive, tasks.finalize_requested, tasks.finalized \
             FROM connection_operations operations \
             JOIN task_submissions tasks ON tasks.task_uid = operations.task_uid \
             WHERE (operations.operation_state NOT IN ('succeeded', 'failed')) \
                OR (operations.finalization_state <> 'finalized') \
                OR (operations.oauth_phase = 'pending' AND operations.flow_expires_at <= now()) \
             ORDER BY operations.created_at, operations.operation_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(database_error)?
        .into_iter()
        .map(connection_operation_record)
            .collect()
    }

    pub async fn connection_operation_deadline_elapsed(
        &self,
        operation_id: Uuid,
    ) -> Result<bool, StoreError> {
        sqlx::query_scalar(
            "SELECT response_deadline_at <= now() FROM connection_operations \
             WHERE operation_id = $1",
        )
        .bind(operation_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(database_error)?
        .ok_or(StoreError::ConnectionOperationNotFound)
    }

    pub async fn bind_task_runtime(
        &self,
        task_uid: Uuid,
        runtime_uid: &str,
        phase: steward_types::TaskPhase,
    ) -> Result<TaskRecord, StoreError> {
        if runtime_uid.is_empty() {
            return Err(StoreError::InvalidTaskTransition);
        }
        let result = sqlx::query(
            "UPDATE task_submissions \
             SET runtime_uid = $2, phase = $3, updated_at = now() \
             WHERE task_uid = $1 AND (runtime_uid IS NULL OR runtime_uid = $2)",
        )
        .bind(task_uid)
        .bind(runtime_uid)
        .bind(task_phase_text(phase))
        .execute(&self.pool)
        .await
        .map_err(database_error)?;
        if result.rows_affected() != 1 {
            return Err(StoreError::InvalidTaskTransition);
        }
        self.task(task_uid).await?.ok_or(StoreError::TaskNotFound)
    }

    pub async fn task(&self, task_uid: Uuid) -> Result<Option<TaskRecord>, StoreError> {
        let row = sqlx::query("SELECT * FROM task_submissions WHERE task_uid = $1")
            .bind(task_uid)
            .fetch_optional(&self.pool)
            .await
            .map_err(database_error)?;
        row.map(task_record).transpose()
    }

    pub async fn put_task_inputs(
        &self,
        task_uid: Uuid,
        submitter_service: &str,
        owner_user_id: &str,
        archive: &[u8],
    ) -> Result<TaskRecord, StoreError> {
        let result = sqlx::query(
            "UPDATE task_submissions \
             SET input_archive = $4, updated_at = now() \
             WHERE task_uid = $1 AND submitter_service = $2 \
               AND owner_user_id = $3 AND identity_binding_state = 'bound' \
               AND phase IN ('submitted', 'parked') \
               AND NOT execute_requested \
               AND (input_archive IS NULL OR input_archive = $4)",
        )
        .bind(task_uid)
        .bind(submitter_service)
        .bind(owner_user_id)
        .bind(archive)
        .execute(&self.pool)
        .await
        .map_err(database_error)?;
        if result.rows_affected() != 1 {
            return Err(StoreError::InvalidTaskTransition);
        }
        self.task(task_uid).await?.ok_or(StoreError::TaskNotFound)
    }

    pub async fn request_task_execution(
        &self,
        task_uid: Uuid,
        submitter_service: &str,
        owner_user_id: &str,
    ) -> Result<TaskRecord, StoreError> {
        let result = sqlx::query(
            "UPDATE task_submissions \
             SET execute_requested = true, \
                 phase = CASE WHEN phase = 'submitted' THEN 'queued' ELSE phase END, \
                 updated_at = now() \
             WHERE task_uid = $1 AND submitter_service = $2 \
               AND owner_user_id = $3 AND identity_binding_state = 'bound' \
               AND input_archive IS NOT NULL \
               AND phase IN ('submitted', 'parked', 'queued') \
               AND NOT finalize_requested",
        )
        .bind(task_uid)
        .bind(submitter_service)
        .bind(owner_user_id)
        .execute(&self.pool)
        .await
        .map_err(database_error)?;
        if result.rows_affected() != 1 {
            return Err(StoreError::InvalidTaskTransition);
        }
        self.task(task_uid).await?.ok_or(StoreError::TaskNotFound)
    }

    pub async fn task_for_submitter(
        &self,
        task_uid: Uuid,
        submitter_service: &str,
        owner_user_id: &str,
    ) -> Result<Option<TaskRecord>, StoreError> {
        let row = sqlx::query(
            "SELECT * FROM task_submissions \
             WHERE task_uid = $1 AND submitter_service = $2 \
               AND owner_user_id = $3 AND identity_binding_state = 'bound' \
               AND NOT EXISTS (SELECT 1 FROM connection_operations operations \
                   WHERE operations.task_uid = task_submissions.task_uid)",
        )
        .bind(task_uid)
        .bind(submitter_service)
        .bind(owner_user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(database_error)?;
        row.map(task_record).transpose()
    }

    pub async fn request_task_finalization(
        &self,
        task_uid: Uuid,
        submitter_service: &str,
        owner_user_id: &str,
    ) -> Result<TaskRecord, StoreError> {
        let result = sqlx::query(
            "UPDATE task_submissions \
             SET finalize_requested = true, \
                 phase = CASE \
                     WHEN phase IN ('submitted', 'parked', 'queued') THEN 'cancelled' \
                     ELSE phase \
                 END, \
                 updated_at = now() \
             WHERE task_uid = $1 AND submitter_service = $2 \
               AND owner_user_id = $3 AND identity_binding_state = 'bound'",
        )
        .bind(task_uid)
        .bind(submitter_service)
        .bind(owner_user_id)
        .execute(&self.pool)
        .await
        .map_err(database_error)?;
        if result.rows_affected() != 1 {
            return Err(StoreError::TaskNotFound);
        }
        self.task(task_uid).await?.ok_or(StoreError::TaskNotFound)
    }

    pub async fn task_work_items(&self) -> Result<Vec<TaskRecord>, StoreError> {
        sqlx::query(
            "SELECT * FROM task_submissions \
             WHERE (runtime_ownership = 'provisioned' AND runtime_uid IS NULL \
                    AND phase IN ('submitted', 'queued') AND NOT finalize_requested) \
                OR (execute_requested AND phase IN ('parked', 'queued')) \
                OR (finalize_requested AND NOT finalized) \
             ORDER BY created_at, task_uid",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(database_error)?
        .into_iter()
        .map(task_record)
        .collect()
    }

    /// Resolve a bridge candidate from server-owned task state only.
    ///
    /// A caller cannot choose a namespace, runtime name, or UID. Ambiguity fails closed so a
    /// bridge cannot accidentally follow an arbitrary concurrent run for the same owner.
    pub async fn active_task_runtime(
        &self,
        owner_user_id: &CanonicalUserId,
        submitter_service: &str,
    ) -> Result<Option<ActiveTaskRuntime>, StoreError> {
        let rows = sqlx::query(
            "SELECT task_uid, runtime_uid, runtime_namespace, runtime_name \
             FROM task_submissions \
             WHERE owner_user_id = $1 AND submitter_service = $2 \
               AND identity_binding_state = 'bound' AND runtime_uid IS NOT NULL \
               AND phase = 'running' AND NOT finalized \
               AND NOT EXISTS (SELECT 1 FROM connection_operations operations \
                   WHERE operations.task_uid = task_submissions.task_uid) \
             ORDER BY created_at, task_uid \
             LIMIT 2",
        )
        .bind(owner_user_id.as_str())
        .bind(submitter_service)
        .fetch_all(&self.pool)
        .await
        .map_err(database_error)?;
        if rows.len() != 1 {
            return Ok(None);
        }
        let row = rows.into_iter().next().ok_or_else(|| {
            StoreError::Database("active task runtime query returned no row".to_owned())
        })?;
        let runtime_uid = row
            .try_get::<String, _>("runtime_uid")
            .map_err(database_error)?;
        if runtime_uid.is_empty() {
            return Err(StoreError::InvalidTaskTransition);
        }
        Ok(Some(ActiveTaskRuntime {
            task_uid: row.try_get("task_uid").map_err(database_error)?,
            runtime_uid,
            runtime_namespace: row.try_get("runtime_namespace").map_err(database_error)?,
            runtime_name: row.try_get("runtime_name").map_err(database_error)?,
        }))
    }

    pub async fn release_parked_task(&self, task_uid: Uuid) -> Result<bool, StoreError> {
        sqlx::query(
            "UPDATE task_submissions SET phase = 'queued', updated_at = now() \
             WHERE task_uid = $1 AND phase = 'parked' AND execute_requested \
               AND NOT finalize_requested",
        )
        .bind(task_uid)
        .execute(&self.pool)
        .await
        .map(|result| result.rows_affected() == 1)
        .map_err(database_error)
    }

    pub async fn claim_task_execution(&self, task_uid: Uuid) -> Result<bool, StoreError> {
        sqlx::query(
            "UPDATE task_submissions SET phase = 'running', updated_at = now() \
             WHERE task_uid = $1 AND phase = 'queued' AND execute_requested \
               AND NOT finalize_requested",
        )
        .bind(task_uid)
        .execute(&self.pool)
        .await
        .map(|result| result.rows_affected() == 1)
        .map_err(database_error)
    }

    pub async fn complete_task_execution(
        &self,
        task_uid: Uuid,
        output_archive: &[u8],
    ) -> Result<(), StoreError> {
        let result = sqlx::query(
            "UPDATE task_submissions \
             SET phase = 'succeeded', output_archive = $2, updated_at = now() \
             WHERE task_uid = $1 AND phase = 'running' AND NOT finalize_requested",
        )
        .bind(task_uid)
        .bind(output_archive)
        .execute(&self.pool)
        .await
        .map_err(database_error)?;
        if result.rows_affected() == 1 {
            Ok(())
        } else {
            Err(StoreError::InvalidTaskTransition)
        }
    }

    pub async fn fail_task_execution(
        &self,
        task_uid: Uuid,
        reason: &str,
    ) -> Result<(), StoreError> {
        if reason.is_empty() {
            return Err(StoreError::InvalidTaskTransition);
        }
        let result = sqlx::query(
            "UPDATE task_submissions \
             SET phase = CASE WHEN finalize_requested THEN 'cancelled' ELSE 'failed' END, \
                 finalize_requested = true, failure_reason = $2, updated_at = now() \
             WHERE task_uid = $1 AND phase = 'running'",
        )
        .bind(task_uid)
        .bind(reason)
        .execute(&self.pool)
        .await
        .map_err(database_error)?;
        if result.rows_affected() == 1 {
            Ok(())
        } else {
            Err(StoreError::InvalidTaskTransition)
        }
    }

    /// Commits a validated provider-control result and finalization request together. Raw bridge
    /// output is cleared in the same transaction so OAuth continuation material cannot remain in
    /// generic task storage after extraction.
    pub async fn complete_connection_operation(
        &self,
        operation_id: Uuid,
        result: &serde_json::Value,
        authorization_url: Option<&str>,
        authorization_url_digest: Option<&str>,
        retention: ConnectionOperationRetention,
    ) -> Result<(), StoreError> {
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let row = sqlx::query(
            "SELECT operation_kind FROM connection_operations \
             WHERE operation_id = $1 FOR UPDATE",
        )
        .bind(operation_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?
        .ok_or(StoreError::ConnectionOperationNotFound)?;
        let operation_kind = connection_operation_kind_from_text(
            &row.try_get::<String, _>("operation_kind")
                .map_err(database_error)?,
        )?;
        if matches!(operation_kind, ConnectionOperationKind::Start) != authorization_url.is_some()
            || authorization_url.is_some() != authorization_url_digest.is_some()
            || retention.cache_ttl_seconds < 0
            || retention.result_ttl_seconds < 0
            || retention.oauth_lifetime_seconds < 0
        {
            return Err(StoreError::InvalidConnectionOperation);
        }
        let updated = sqlx::query(
            "UPDATE connection_operations \
             SET operation_state = 'succeeded', \
                 result = CASE WHEN operation_kind = 'start' \
                     THEN '{\"started\":true}'::jsonb ELSE $2 END, \
                 cached_status = CASE WHEN operation_kind = 'status' THEN $2 ELSE cached_status END, \
                 cache_expires_at = CASE WHEN operation_kind = 'status' \
                     THEN now() + make_interval(secs => $5) ELSE NULL END, \
                 result_expires_at = CASE WHEN operation_kind = 'disconnect' \
                     THEN now() + make_interval(secs => $6) ELSE result_expires_at END, \
                 oauth_phase = CASE WHEN operation_kind = 'start' THEN 'pending' ELSE oauth_phase END, \
                 authorization_url = $3, authorization_url_digest = $4, \
                 flow_created_at = CASE WHEN operation_kind = 'start' THEN now() ELSE flow_created_at END, \
                 flow_expires_at = CASE WHEN operation_kind = 'start' \
                     THEN now() + make_interval(secs => $7) ELSE flow_expires_at END, \
                 finalization_state = 'requested', cleanup_state = 'tearing_down', \
                 failure_category = NULL, updated_at = now() \
             WHERE operation_id = $1 AND operation_state NOT IN ('succeeded', 'failed')",
        )
        .bind(operation_id)
        .bind(Json(result))
        .bind(authorization_url)
        .bind(authorization_url_digest)
        .bind(retention.cache_ttl_seconds as f64)
        .bind(retention.result_ttl_seconds as f64)
        .bind(retention.oauth_lifetime_seconds as f64)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        if updated.rows_affected() != 1 {
            return Err(StoreError::InvalidConnectionOperation);
        }
        if operation_kind != ConnectionOperationKind::Status {
            sqlx::query(
                "UPDATE connection_operations \
                 SET cached_status = NULL, cache_expires_at = NULL, updated_at = now() \
                 WHERE canonical_user_id = (SELECT canonical_user_id \
                         FROM connection_operations WHERE operation_id = $1) \
                   AND provider = 'github' AND operation_kind = 'status'",
            )
            .bind(operation_id)
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
        }
        let finalized = sqlx::query(
            "UPDATE task_submissions \
             SET output_archive = NULL, finalize_requested = true, updated_at = now() \
             WHERE task_uid = $1",
        )
        .bind(operation_id)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        if finalized.rows_affected() != 1 {
            return Err(StoreError::ConnectionOperationNotFound);
        }
        transaction.commit().await.map_err(database_error)
    }

    pub async fn fail_connection_operation(
        &self,
        operation_id: Uuid,
        category: &str,
    ) -> Result<(), StoreError> {
        if category.trim().is_empty() {
            return Err(StoreError::InvalidConnectionOperation);
        }
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let operation = sqlx::query(
            "UPDATE connection_operations \
             SET operation_state = 'failed', failure_category = $2, result = NULL, \
                 authorization_url = NULL, finalization_state = 'requested', \
                 cleanup_state = 'tearing_down', updated_at = now() \
             WHERE operation_id = $1 AND operation_state NOT IN ('succeeded', 'failed') \
             RETURNING task_uid",
        )
        .bind(operation_id)
        .bind(category)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?
        .ok_or(StoreError::InvalidConnectionOperation)?;
        let task_uid = operation
            .try_get::<Uuid, _>("task_uid")
            .map_err(database_error)?;
        sqlx::query(
            "UPDATE task_submissions \
             SET phase = CASE WHEN phase IN ('succeeded', 'failed') THEN phase ELSE 'failed' END, \
                 output_archive = NULL, finalize_requested = true, failure_reason = $2, \
                 updated_at = now() WHERE task_uid = $1",
        )
        .bind(task_uid)
        .bind(category)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        transaction.commit().await.map_err(database_error)
    }

    pub async fn expire_connection_oauth_flow(
        &self,
        operation_id: Uuid,
    ) -> Result<bool, StoreError> {
        sqlx::query(
            "UPDATE connection_operations \
             SET oauth_phase = 'expired', authorization_url = NULL, updated_at = now() \
             WHERE operation_id = $1 AND oauth_phase = 'pending' \
               AND flow_expires_at <= now()",
        )
        .bind(operation_id)
        .execute(&self.pool)
        .await
        .map(|result| result.rows_affected() == 1)
        .map_err(database_error)
    }

    pub async fn complete_pending_connection_oauth_flow(
        &self,
        canonical_user_id: &CanonicalUserId,
    ) -> Result<bool, StoreError> {
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("connection:{}:github", canonical_user_id.as_str()))
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
        let updated = sqlx::query(
            "UPDATE connection_operations \
             SET oauth_phase = 'completed', authorization_url = NULL, updated_at = now() \
             WHERE operation_id = ( \
               SELECT operation_id FROM connection_operations \
               WHERE canonical_user_id = $1 AND provider = 'github' \
                 AND oauth_phase = 'pending' AND flow_expires_at > now() \
               ORDER BY created_at DESC LIMIT 1 FOR UPDATE)",
        )
        .bind(canonical_user_id.as_str())
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        if updated.rows_affected() == 1 {
            sqlx::query(
                "UPDATE connection_operations \
                 SET cached_status = NULL, cache_expires_at = NULL, updated_at = now() \
                 WHERE canonical_user_id = $1 AND provider = 'github' \
                   AND operation_kind = 'status'",
            )
            .bind(canonical_user_id.as_str())
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
        }
        transaction.commit().await.map_err(database_error)?;
        Ok(updated.rows_affected() == 1)
    }

    pub async fn reconcile_connection_cleanup_state(
        &self,
        operation_id: Uuid,
        task_finalized: bool,
    ) -> Result<(), StoreError> {
        let result = sqlx::query(
            "UPDATE connection_operations \
             SET finalization_state = CASE WHEN $2 THEN 'finalized' ELSE 'requested' END, \
                 cleanup_state = CASE WHEN $2 THEN 'clean' ELSE 'tearing_down' END, \
                 updated_at = now() \
             WHERE operation_id = $1",
        )
        .bind(operation_id)
        .bind(task_finalized)
        .execute(&self.pool)
        .await
        .map_err(database_error)?;
        if result.rows_affected() == 1 {
            Ok(())
        } else {
            Err(StoreError::ConnectionOperationNotFound)
        }
    }

    pub async fn mark_stalled_connection_cleanup(
        &self,
        operation_id: Uuid,
        grace_seconds: i64,
    ) -> Result<bool, StoreError> {
        if grace_seconds <= 0 {
            return Err(StoreError::InvalidConnectionOperation);
        }
        sqlx::query(
            "UPDATE connection_operations operations \
             SET cleanup_state = 'stalled', cleanup_finding = 'teardown_stalled', \
                 updated_at = now() \
             FROM task_submissions tasks \
             WHERE operations.operation_id = $1 \
               AND tasks.task_uid = operations.task_uid \
               AND operations.finalization_state = 'requested' \
               AND operations.cleanup_state = 'tearing_down' \
               AND NOT tasks.finalized \
               AND operations.updated_at + make_interval(secs => $2) <= now()",
        )
        .bind(operation_id)
        .bind(grace_seconds as f64)
        .execute(&self.pool)
        .await
        .map(|result| result.rows_affected() == 1)
        .map_err(database_error)
    }

    pub async fn mark_task_finalized(&self, task_uid: Uuid) -> Result<(), StoreError> {
        let result = sqlx::query(
            "UPDATE task_submissions SET finalized = true, updated_at = now() \
             WHERE task_uid = $1 AND finalize_requested AND NOT finalized",
        )
        .bind(task_uid)
        .execute(&self.pool)
        .await
        .map_err(database_error)?;
        if result.rows_affected() == 1 {
            Ok(())
        } else {
            Err(StoreError::InvalidTaskTransition)
        }
    }

    pub async fn task_by_idempotency(
        &self,
        submitter_service: &str,
        owner_user_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<TaskRecord>, StoreError> {
        let row = sqlx::query(
            "SELECT * FROM task_submissions \
             WHERE submitter_service = $1 AND owner_user_id = $2 \
               AND idempotency_key = $3 AND identity_binding_state = 'bound'",
        )
        .bind(submitter_service)
        .bind(owner_user_id)
        .bind(idempotency_key)
        .fetch_optional(&self.pool)
        .await
        .map_err(database_error)?;
        row.map(task_record).transpose()
    }
}

pub struct TaskReservationRequest<'a> {
    pub idempotency_key: &'a str,
    pub submitter_service: &'a str,
    pub acting_user: Option<&'a str>,
    pub acting_user_id: Option<&'a str>,
    pub owner: &'a str,
    pub owner_user_id: &'a str,
    pub workflow: &'a str,
    pub workflow_name: Option<&'a str>,
    pub workflow_version: Option<i64>,
    pub workflow_digest: Option<&'a str>,
    pub user_envelope_instance_id: Option<&'a str>,
    pub user_envelope_revision: Option<i64>,
    pub user_envelope_digest: Option<&'a str>,
    pub coding_agent_runtime: &'a str,
    pub runtime_namespace: &'a str,
    pub runtime_name: &'a str,
    pub runtime_ownership: steward_types::RuntimeOwnership,
    pub runtime_spec: &'a AgentRuntimeSpec,
    pub agent_command: &'a [String],
    pub execution_binding: Option<&'a TaskExecutionBinding>,
    pub envelope_revision: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentRunQuery {
    pub limit: u16,
    pub cursor: Option<Uuid>,
    pub phase: Option<steward_types::TaskPhase>,
    pub workflow: Option<String>,
    /// Exact server-derived canonical owner scope. `None` is reserved for administrator reads.
    pub owner_user_id: Option<String>,
    /// Exact Kubernetes runtime binding.
    pub runtime_uid: Option<String>,
    /// Exact envelope instance selected and persisted at task admission.
    pub user_envelope_instance_id: Option<String>,
    /// Exact durable task identity, used for a single run detail read.
    pub task_uid: Option<Uuid>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentRunPage {
    pub records: Vec<AgentRunRecord>,
    pub next_cursor: Option<Uuid>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentRunRecord {
    pub task_uid: Uuid,
    pub submitter_service: String,
    pub acting_user: Option<String>,
    pub owner: String,
    pub owner_user_id: Option<String>,
    pub workflow: String,
    pub workflow_name: Option<String>,
    pub workflow_version: Option<i64>,
    pub workflow_digest: Option<String>,
    pub user_envelope_instance_id: Option<String>,
    pub user_envelope_revision: Option<i64>,
    pub user_envelope_digest: Option<String>,
    pub coding_agent_runtime: String,
    pub runtime_uid: Option<String>,
    pub runtime_ownership: steward_types::RuntimeOwnership,
    pub phase: steward_types::TaskPhase,
    pub runtime_spec: AgentRuntimeSpec,
    pub envelope_revision: Option<i64>,
    pub finalize_requested: bool,
    pub finalized: bool,
    pub failure_reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub spend: Option<AgentRunSpend>,
    pub history_partial: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentRunSpend {
    pub observed_amount: String,
    pub currency: String,
    pub exhausted: bool,
    pub observed_at: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentRunTimelineKind {
    Phase(steward_types::TaskPhase),
    FinalizationRequested,
    Finalized,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentRunTimelineProvenance {
    Recorded,
    Backfilled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentRunTimelineEvent {
    pub kind: AgentRunTimelineKind,
    pub provenance: AgentRunTimelineProvenance,
    pub at: String,
}

/// User-visible status derived from the latest append-only request event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnvelopeRequestStatus {
    Pending,
    Approved,
    Rejected,
    Provisioned,
    Stale,
    Conflict,
}

impl EnvelopeRequestStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Provisioned => "provisioned",
            Self::Stale => "stale",
            Self::Conflict => "conflict",
        }
    }
}

/// Immutable request fact plus its latest server-authoritative status event.
#[derive(Clone, Debug, PartialEq)]
pub struct EnvelopeRequestRecord {
    pub id: Uuid,
    pub owner_user_id: CanonicalUserId,
    pub template_id: String,
    pub template_revision: i64,
    pub requested_envelope: Envelope,
    pub approved_envelope: Option<Envelope>,
    pub status: EnvelopeRequestStatus,
    pub approval_id: Option<Uuid>,
    pub envelope_instance_id: Option<String>,
    pub envelope_digest: Option<String>,
    pub reason: Option<String>,
    pub status_actor: String,
    pub status_template_revision: i64,
    pub created_at: String,
    pub status_at: String,
}

pub struct EnvelopeRequestReservationRequest<'a> {
    pub owner_user_id: &'a CanonicalUserId,
    pub template_id: &'a str,
    pub template_revision: i64,
    pub requested_envelope: &'a Envelope,
    pub idempotency_key: &'a str,
    pub actor: &'a str,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnvelopeRequestReservation {
    pub inserted: bool,
    pub record: EnvelopeRequestRecord,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PendingEnvelopeRequest {
    pub request_id: Uuid,
    pub owner_display_email: String,
    pub template_id: String,
    pub template_revision: i64,
    pub requested_envelope: Envelope,
    pub template_envelope: Envelope,
    pub created_at: String,
}

pub struct EnvelopeRequestStatusUpdate<'a> {
    pub from: EnvelopeRequestStatus,
    pub to: EnvelopeRequestStatus,
    pub approval_id: Option<Uuid>,
    pub envelope_instance_id: Option<&'a str>,
    pub envelope_digest: Option<&'a str>,
    pub reason: Option<&'a str>,
    pub approved_envelope: Option<&'a Envelope>,
    pub actor: &'a str,
}

fn validate_task_identity_binding(request: &TaskReservationRequest<'_>) -> Result<(), StoreError> {
    let owner_user_id = CanonicalUserId::parse(request.owner_user_id)
        .map_err(|_| StoreError::InvalidTaskIdentityBinding)?;
    let acting_user_id = request
        .acting_user_id
        .map(CanonicalUserId::parse)
        .transpose()
        .map_err(|_| StoreError::InvalidTaskIdentityBinding)?;
    if request.acting_user.is_some() != acting_user_id.is_some()
        || acting_user_id
            .as_ref()
            .is_some_and(|acting_user_id| acting_user_id != &owner_user_id)
    {
        return Err(StoreError::InvalidTaskIdentityBinding);
    }
    let authority = request
        .runtime_spec
        .canonical_authority
        .as_ref()
        .ok_or(StoreError::InvalidTaskIdentityBinding)?;
    if authority.owner_user_id != owner_user_id || authority.acting_user_id != acting_user_id {
        return Err(StoreError::InvalidTaskIdentityBinding);
    }
    Ok(())
}

fn validate_connection_operation_request(
    request: &ConnectionOperationReservationRequest<'_>,
) -> Result<(), StoreError> {
    validate_task_identity_binding(&request.task)?;
    validate_task_version_pins(&request.task)?;
    let expected_action = request.operation_kind.as_str();
    let [tool] = request.task.runtime_spec.tools.as_slice() else {
        return Err(StoreError::InvalidConnectionOperation);
    };
    let expected_command = [
        "/usr/local/bin/steward-connections-bridge",
        "--operation",
        match request.operation_kind {
            ConnectionOperationKind::Status => "github.status",
            ConnectionOperationKind::Start => "github.start",
            ConnectionOperationKind::Disconnect => "github.disconnect",
        },
        "--input",
        "request.json",
    ];
    let principal_is_bound = matches!(
        &request.task.runtime_spec.principal,
        steward_types::Principal::Service { name, acting_user }
            if name == "steward-connections"
                && acting_user.as_ref().map(|email| email.as_str()) == request.task.acting_user
    );
    if request.task.submitter_service != "steward-connections"
        || request.authority_id != "steward-connections"
        || request.authority_version != 1
        || request.authority_digest
            != steward_admission::internal_authorities::steward_connections_v1::AUTHORITY_DIGEST
        || request.response_deadline_seconds <= 0
        || request.response_deadline_seconds > 60
        || request.idempotency_identity.trim().is_empty()
        || (request.operation_kind != ConnectionOperationKind::Status
            && !request.allow_status_cache)
        || request.input_archive.is_empty()
        || request.task.runtime_ownership != steward_types::RuntimeOwnership::Provisioned
        || request.task.runtime_spec.agent_type.name != "connections-bridge"
        || !request.task.runtime_spec.llms.is_empty()
        || tool.provider != "github"
        || tool.resource != "provider-control"
        || tool.action != expected_action
        || request
            .task
            .agent_command
            .iter()
            .map(String::as_str)
            .ne(expected_command)
        || request.task.runtime_namespace != request.bindings.namespace
        || !match request.bindings.artifact_trust_mode.as_str() {
            "github-attestation" => {
                valid_digest_pinned_image(&request.bindings.bridge_image_digest)
            }
            "operator-pinned" => valid_operator_pinned_image(&request.bindings.bridge_image_digest),
            _ => false,
        }
        || request.bindings.mcp_gw_origin.trim().is_empty()
        || request.bindings.mcp_gw_version != "0.3.2"
        || request.bindings.namespace.trim().is_empty()
        || request.bindings.runtime_class.trim().is_empty()
        || !principal_is_bound
    {
        return Err(StoreError::InvalidConnectionOperation);
    }
    Ok(())
}

fn valid_sha256_reference(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn valid_digest_pinned_image(value: &str) -> bool {
    value.split_once("@").is_some_and(|(repository, digest)| {
        !repository.is_empty() && valid_sha256_reference(digest)
    })
}

fn valid_operator_pinned_image(value: &str) -> bool {
    if value
        .bytes()
        .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        || value.matches('@').count() != 1
        || value.contains("://")
    {
        return false;
    }
    let Some((repository, digest)) = value.split_once('@') else {
        return false;
    };
    let mut components = repository.split('/');
    let Some(registry) = components.next() else {
        return false;
    };
    let (registry, port) = registry
        .split_once(':')
        .map_or((registry, None), |(registry, port)| (registry, Some(port)));
    let valid_component = |component: &str| {
        !component.is_empty()
            && component.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            })
    };
    if !valid_component(registry)
        || port
            .is_some_and(|port| port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()))
        || components.any(|component| !valid_component(component))
    {
        return false;
    }
    digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn connection_execution_bindings_match(
    persisted: &ConnectionExecutionBindingSnapshot,
    current: &ConnectionExecutionBindingSnapshot,
) -> bool {
    persisted.artifact_trust_mode == current.artifact_trust_mode
        && persisted.bridge_image_digest == current.bridge_image_digest
        && persisted.mcp_gw_origin == current.mcp_gw_origin
        && persisted.mcp_gw_version == current.mcp_gw_version
        && persisted.namespace == current.namespace
        && persisted.runtime_class == current.runtime_class
}

fn validate_task_version_pins(request: &TaskReservationRequest<'_>) -> Result<(), StoreError> {
    let workflow_pins = [
        request.workflow_name.is_some(),
        request.workflow_version.is_some(),
        request.workflow_digest.is_some(),
    ];
    let envelope_pins = [
        request.user_envelope_instance_id.is_some(),
        request.user_envelope_revision.is_some(),
        request.user_envelope_digest.is_some(),
    ];
    let complete = |pins: [bool; 3]| {
        pins.iter().all(|present| *present) || pins.iter().all(|present| !*present)
    };
    if request
        .execution_binding
        .is_some_and(|binding| binding.validate().is_err())
        || request.execution_binding.is_some_and(|binding| {
            binding
                .disposable()
                .is_some_and(|disposable| disposable.agent_ref != request.coding_agent_runtime)
        })
        || request
            .execution_binding
            .is_some_and(|binding| match binding {
                TaskExecutionBinding::Disposable(_) => {
                    request.runtime_ownership != steward_types::RuntimeOwnership::Provisioned
                }
                TaskExecutionBinding::Resident(resident) => {
                    request.runtime_ownership != steward_types::RuntimeOwnership::Adopted
                        || resident.owner_user_id.as_str() != request.owner_user_id
                }
            })
        || !complete(workflow_pins)
        || !complete(envelope_pins)
        || workflow_pins[0] != envelope_pins[0]
        || request.workflow_version.is_some_and(|version| version <= 0)
        || request
            .user_envelope_revision
            .is_some_and(|revision| revision <= 0)
        || request.workflow_name.is_some_and(str::is_empty)
        || request.workflow_digest.is_some_and(str::is_empty)
        || request.user_envelope_instance_id.is_some_and(str::is_empty)
        || request.user_envelope_digest.is_some_and(str::is_empty)
    {
        return Err(StoreError::InvalidTaskIdentityBinding);
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub struct TaskReservation {
    pub inserted: bool,
    pub record: TaskRecord,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TaskRecord {
    pub task_uid: Uuid,
    pub idempotency_key: String,
    pub submitter_service: String,
    pub acting_user: Option<String>,
    pub acting_user_id: Option<String>,
    pub owner: String,
    pub owner_user_id: Option<String>,
    pub identity_binding_state: String,
    pub workflow: String,
    pub workflow_name: Option<String>,
    pub workflow_version: Option<i64>,
    pub workflow_digest: Option<String>,
    pub user_envelope_instance_id: Option<String>,
    pub user_envelope_revision: Option<i64>,
    pub user_envelope_digest: Option<String>,
    pub internal_authority_id: Option<String>,
    pub internal_authority_version: Option<i64>,
    pub internal_authority_digest: Option<String>,
    pub coding_agent_runtime: String,
    pub runtime_uid: Option<String>,
    pub runtime_namespace: String,
    pub runtime_name: String,
    pub runtime_ownership: steward_types::RuntimeOwnership,
    pub phase: steward_types::TaskPhase,
    pub runtime_spec: AgentRuntimeSpec,
    pub agent_command: Vec<String>,
    pub execution_binding: Option<TaskExecutionBinding>,
    pub input_archive: Option<Vec<u8>>,
    pub output_archive: Option<Vec<u8>>,
    pub execute_requested: bool,
    pub finalize_requested: bool,
    pub finalized: bool,
    pub failure_reason: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionOperationKind {
    Status,
    Start,
    Disconnect,
}

impl ConnectionOperationKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Start => "start",
            Self::Disconnect => "disconnect",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionOperationState {
    Queued,
    Provisioning,
    Running,
    Succeeded,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionOAuthPhase {
    None,
    Pending,
    Completed,
    Expired,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectionExecutionBindingSnapshot {
    pub artifact_trust_mode: String,
    pub bridge_image_digest: String,
    pub mcp_gw_origin: String,
    pub mcp_gw_version: String,
    pub namespace: String,
    pub runtime_class: String,
}

pub struct ConnectionOperationReservationRequest<'a> {
    pub operation_id: Uuid,
    pub operation_kind: ConnectionOperationKind,
    pub authority_id: &'a str,
    pub authority_version: i64,
    pub authority_digest: &'a str,
    pub bindings: &'a ConnectionExecutionBindingSnapshot,
    pub idempotency_identity: &'a str,
    pub response_deadline_seconds: i64,
    /// Status-only cache control. False forces a new status operation while still joining an
    /// identical in-flight status. Mutating operations must always set this to true.
    pub allow_status_cache: bool,
    pub input_archive: &'a [u8],
    pub task: TaskReservationRequest<'a>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectionOperationRetention {
    pub cache_ttl_seconds: i64,
    pub result_ttl_seconds: i64,
    pub oauth_lifetime_seconds: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConnectionOperationRecord {
    pub operation_id: Uuid,
    pub task_uid: Uuid,
    pub canonical_user_id: String,
    pub provider: String,
    pub operation_kind: ConnectionOperationKind,
    pub authority_id: String,
    pub authority_version: i64,
    pub authority_digest: String,
    pub runtime_spec_snapshot: AgentRuntimeSpec,
    pub command_snapshot: Vec<String>,
    pub bindings: ConnectionExecutionBindingSnapshot,
    pub idempotency_identity: String,
    pub uncached_status: bool,
    pub operation_state: ConnectionOperationState,
    pub oauth_phase: ConnectionOAuthPhase,
    /// Sensitive transient continuation. Never expose through generic read models or logs.
    pub authorization_url: Option<String>,
    pub authorization_url_digest: Option<String>,
    pub flow_expires_at: Option<String>,
    pub cached_status: Option<serde_json::Value>,
    pub result: Option<serde_json::Value>,
    pub failure_category: Option<String>,
    pub finalization_state: String,
    pub cleanup_state: String,
    pub cleanup_finding: Option<String>,
    pub response_deadline_at: String,
    pub task_phase: steward_types::TaskPhase,
    pub runtime_uid: Option<String>,
    pub output_archive: Option<Vec<u8>>,
    pub finalize_requested: bool,
    pub finalized: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConnectionOperationReservation {
    pub inserted: bool,
    pub record: ConnectionOperationRecord,
}

/// The sole durable candidate a stable bridge may inspect before it validates the live object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveTaskRuntime {
    pub task_uid: Uuid,
    pub runtime_uid: String,
    pub runtime_namespace: String,
    pub runtime_name: String,
}

pub struct ParkRejection<'a> {
    pub runtime_uid: &'a str,
    pub runtime_namespace: &'a str,
    pub runtime_name: &'a str,
    pub spec_digest: &'a str,
    pub base_spec_digest: &'a str,
    pub base_pending_approval_digest: Option<&'a str>,
    pub base_spec: &'a AgentRuntimeSpec,
    pub envelope_revision: i64,
    pub deltas: &'a [AdmissionDelta],
    pub proposed_spec: &'a AgentRuntimeSpec,
    pub actor: &'a str,
    pub member_role: &'a str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParkedAdmission {
    pub decision_id: Uuid,
    pub approval_id: Uuid,
    pub decision_key: Option<String>,
    pub evidence_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PendingApproval {
    pub approval_id: Uuid,
    pub decision_id: Uuid,
    pub runtime_uid: String,
    pub decision_key: Option<String>,
    pub evidence_url: Option<String>,
    pub deltas: Vec<AdmissionDelta>,
    pub proposed_spec: AgentRuntimeSpec,
    pub base_spec_digest: String,
    pub base_pending_approval_digest: Option<String>,
    pub envelope_revision: i64,
    pub actor: String,
    pub member_role: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ApprovalCandidate {
    pub approval_id: Uuid,
    pub runtime_uid: String,
    pub proposed_spec: AgentRuntimeSpec,
    pub base_spec_digest: String,
    pub base_pending_approval_digest: Option<String>,
    pub actor: String,
    pub member_role: String,
    pub envelope_revision: i64,
    pub runtime_namespace: String,
    pub runtime_name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DecisionFiling {
    pub approval_id: Uuid,
    pub runtime_uid: String,
    pub actor: String,
    pub member_role: String,
    pub deltas: Vec<AdmissionDelta>,
    pub decision_key: Option<String>,
    pub evidence_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DecisionFilingClaim {
    pub filing: DecisionFiling,
    pub token: Option<Uuid>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GrantReversion {
    pub runtime_uid: String,
    pub runtime_namespace: String,
    pub runtime_name: String,
    pub actor: String,
    pub member_role: String,
    pub base_spec: AgentRuntimeSpec,
    pub proposed_spec: AgentRuntimeSpec,
    pub base_pending_approval_digest: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GrantApplication {
    pub approval_id: Uuid,
    pub application: GrantReversion,
}

pub struct ApproveAdmission<'a> {
    pub approval_id: Uuid,
    pub decided_by: &'a str,
    pub rationale: &'a str,
    pub evidence_url: &'a str,
    pub expires_at: &'a str,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ApprovedAdmission {
    pub approval_id: Uuid,
    pub decision_id: Uuid,
    pub runtime_uid: String,
    pub proposed_spec: AgentRuntimeSpec,
    pub base_spec_digest: String,
    pub actor: String,
    pub member_role: String,
    pub decision_key: String,
    pub evidence_url: String,
    pub grants: Vec<AdmissionDelta>,
    pub decided_by: String,
    pub rationale: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoreError {
    Database(String),
    CanonicalIdentityNotFound,
    CanonicalIdentityInactive,
    CanonicalIdentityStale,
    CanonicalIdentityAmbiguousEmail,
    CanonicalIdentityConflict,
    CanonicalIdentityInvalidActor,
    CanonicalIdentityInvalidRecord,
    InvalidBrowserRbacActor,
    InvalidBrowserRbacAssignment,
    InvalidBrowserRbacRecord,
    ApprovalNotFound,
    ApprovalNotPending,
    MissingDecisionReference,
    DecisionReferenceMismatch,
    DecisionFilingInProgress,
    DecisionFilingClaimLost,
    EvidenceMismatch,
    InvalidGrantExpiry,
    MissingRevocationReason,
    StaleEnvelope,
    EnvelopeRevisionNotIncreasing,
    TaskNotFound,
    TaskIdempotencyConflict,
    InvalidTaskIdentityBinding,
    InvalidTaskTransition,
    ConnectionOperationNotFound,
    ConnectionOperationConflict,
    ConnectionOAuthFlowPending,
    InvalidConnectionOperation,
    InvalidRunQuery,
    InvalidRunCursor,
    EnvelopeRequestNotFound,
    EnvelopeRequestIdempotencyConflict,
    EnvelopeRequestTemplateStale,
    InvalidEnvelopeRequest,
    InvalidEnvelopeRequestTransition,
    WorkflowNotFound,
    WorkflowAlreadyExists,
    InvalidWorkflow,
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(reason) => write!(formatter, "Postgres operation failed: {reason}"),
            Self::CanonicalIdentityNotFound => write!(formatter, "canonical identity not found"),
            Self::CanonicalIdentityInactive => write!(formatter, "canonical identity is inactive"),
            Self::CanonicalIdentityStale => {
                write!(
                    formatter,
                    "canonical identity requires explicit reconnection"
                )
            }
            Self::CanonicalIdentityAmbiguousEmail => {
                write!(
                    formatter,
                    "verified email is already bound in this organization"
                )
            }
            Self::CanonicalIdentityConflict => {
                write!(
                    formatter,
                    "canonical identity mapping conflicts with an existing mapping"
                )
            }
            Self::CanonicalIdentityInvalidActor => {
                write!(
                    formatter,
                    "canonical identity change requires an audited actor"
                )
            }
            Self::CanonicalIdentityInvalidRecord => {
                write!(formatter, "canonical identity record is invalid")
            }
            Self::InvalidBrowserRbacActor => {
                write!(formatter, "browser RBAC actor is required")
            }
            Self::InvalidBrowserRbacAssignment => {
                write!(formatter, "browser RBAC assignment is invalid")
            }
            Self::InvalidBrowserRbacRecord => {
                write!(formatter, "browser RBAC record is invalid")
            }
            Self::WorkflowNotFound => write!(formatter, "Workflow revision does not exist"),
            Self::WorkflowAlreadyExists => write!(formatter, "Workflow already exists"),
            Self::InvalidWorkflow => write!(formatter, "Workflow publication is invalid"),
            Self::ApprovalNotFound => write!(formatter, "approval does not exist"),
            Self::ApprovalNotPending => write!(formatter, "approval is not pending"),
            Self::MissingDecisionReference => {
                write!(formatter, "approval has no decision-channel reference")
            }
            Self::DecisionReferenceMismatch => {
                write!(
                    formatter,
                    "approval already has a different decision-channel reference"
                )
            }
            Self::DecisionFilingInProgress => {
                write!(formatter, "decision-channel filing is already in progress")
            }
            Self::DecisionFilingClaimLost => {
                write!(formatter, "decision-channel filing claim was lost")
            }
            Self::EvidenceMismatch => {
                write!(
                    formatter,
                    "approval evidence does not match its channel reference"
                )
            }
            Self::InvalidGrantExpiry => {
                write!(formatter, "grant expiry must be a valid future timestamp")
            }
            Self::MissingRevocationReason => {
                write!(formatter, "grant revocation reason is required")
            }
            Self::StaleEnvelope => {
                write!(formatter, "approval envelope is no longer current")
            }
            Self::EnvelopeRevisionNotIncreasing => {
                write!(formatter, "envelope revision must increase monotonically")
            }
            Self::TaskNotFound => write!(formatter, "task does not exist"),
            Self::TaskIdempotencyConflict => {
                write!(
                    formatter,
                    "idempotency key is already bound to another task request"
                )
            }
            Self::InvalidTaskIdentityBinding => {
                write!(formatter, "task canonical identity binding is invalid")
            }
            Self::InvalidTaskTransition => {
                write!(formatter, "task lifecycle transition is invalid")
            }
            Self::ConnectionOperationNotFound => {
                write!(formatter, "connection operation does not exist")
            }
            Self::ConnectionOperationConflict => {
                write!(
                    formatter,
                    "connection operation conflicts with an active mutation"
                )
            }
            Self::ConnectionOAuthFlowPending => {
                write!(formatter, "OAuth flow remains pending")
            }
            Self::InvalidConnectionOperation => {
                write!(formatter, "connection operation is invalid")
            }
            Self::InvalidRunQuery => write!(formatter, "agent-run query is invalid"),
            Self::InvalidRunCursor => write!(formatter, "agent-run cursor is invalid"),
            Self::EnvelopeRequestNotFound => write!(formatter, "envelope request does not exist"),
            Self::EnvelopeRequestIdempotencyConflict => {
                write!(
                    formatter,
                    "idempotency key is already bound to another envelope request"
                )
            }
            Self::EnvelopeRequestTemplateStale => {
                write!(formatter, "envelope request template revision is stale")
            }
            Self::InvalidEnvelopeRequest => write!(formatter, "envelope request is invalid"),
            Self::InvalidEnvelopeRequestTransition => {
                write!(formatter, "envelope request transition is invalid")
            }
        }
    }
}

impl Error for StoreError {}

fn database_error(error: sqlx::Error) -> StoreError {
    StoreError::Database(error.to_string())
}

fn canonical_identity_database_error(error: sqlx::Error) -> StoreError {
    if error
        .as_database_error()
        .and_then(|error| error.code())
        .as_deref()
        == Some("23505")
    {
        StoreError::CanonicalIdentityConflict
    } else {
        database_error(error)
    }
}

fn canonical_principal_from_row(
    row: &sqlx::postgres::PgRow,
    expected_organization_id: &OrganizationId,
    expected_verified_email: &Email,
) -> Result<CanonicalPrincipal, StoreError> {
    let state: String = row.try_get("state").map_err(database_error)?;
    if state != "active" {
        return Err(StoreError::CanonicalIdentityInactive);
    }
    let user_organization_id = row
        .try_get::<String, _>("user_organization_id")
        .map_err(database_error)
        .and_then(|value| {
            OrganizationId::parse(value).map_err(|_| StoreError::CanonicalIdentityInvalidRecord)
        })?;
    let subject_organization_id = row
        .try_get::<String, _>("subject_organization_id")
        .map_err(database_error)
        .and_then(|value| {
            OrganizationId::parse(value).map_err(|_| StoreError::CanonicalIdentityInvalidRecord)
        })?;
    if user_organization_id != subject_organization_id
        || &user_organization_id != expected_organization_id
    {
        return Err(StoreError::CanonicalIdentityInvalidRecord);
    }
    let display_email: String = row.try_get("display_email").map_err(database_error)?;
    let verified_email: String = row.try_get("verified_email").map_err(database_error)?;
    if !display_email.eq_ignore_ascii_case(expected_verified_email.as_str())
        || !verified_email.eq_ignore_ascii_case(expected_verified_email.as_str())
    {
        return Err(StoreError::CanonicalIdentityStale);
    }
    let user_id = row
        .try_get::<String, _>("user_id")
        .map_err(database_error)
        .and_then(|value| {
            CanonicalUserId::parse(value).map_err(|_| StoreError::CanonicalIdentityInvalidRecord)
        })?;
    CanonicalPrincipal::new(user_id, user_organization_id, Email(display_email))
        .map_err(|_| StoreError::CanonicalIdentityInvalidRecord)
}

fn connection_operation_kind_from_text(value: &str) -> Result<ConnectionOperationKind, StoreError> {
    match value {
        "status" => Ok(ConnectionOperationKind::Status),
        "start" => Ok(ConnectionOperationKind::Start),
        "disconnect" => Ok(ConnectionOperationKind::Disconnect),
        _ => Err(StoreError::InvalidConnectionOperation),
    }
}

fn connection_operation_state_from_text(
    value: &str,
) -> Result<ConnectionOperationState, StoreError> {
    match value {
        "queued" => Ok(ConnectionOperationState::Queued),
        "provisioning" => Ok(ConnectionOperationState::Provisioning),
        "running" => Ok(ConnectionOperationState::Running),
        "succeeded" => Ok(ConnectionOperationState::Succeeded),
        "failed" => Ok(ConnectionOperationState::Failed),
        _ => Err(StoreError::InvalidConnectionOperation),
    }
}

fn connection_oauth_phase_from_text(value: &str) -> Result<ConnectionOAuthPhase, StoreError> {
    match value {
        "none" => Ok(ConnectionOAuthPhase::None),
        "pending" => Ok(ConnectionOAuthPhase::Pending),
        "completed" => Ok(ConnectionOAuthPhase::Completed),
        "expired" => Ok(ConnectionOAuthPhase::Expired),
        _ => Err(StoreError::InvalidConnectionOperation),
    }
}

fn connection_operation_record(
    row: sqlx::postgres::PgRow,
) -> Result<ConnectionOperationRecord, StoreError> {
    Ok(ConnectionOperationRecord {
        operation_id: row.try_get("operation_id").map_err(database_error)?,
        task_uid: row.try_get("task_uid").map_err(database_error)?,
        canonical_user_id: row.try_get("canonical_user_id").map_err(database_error)?,
        provider: row.try_get("provider").map_err(database_error)?,
        operation_kind: connection_operation_kind_from_text(
            &row.try_get::<String, _>("operation_kind")
                .map_err(database_error)?,
        )?,
        authority_id: row.try_get("authority_id").map_err(database_error)?,
        authority_version: row.try_get("authority_version").map_err(database_error)?,
        authority_digest: row.try_get("authority_digest").map_err(database_error)?,
        runtime_spec_snapshot: row
            .try_get::<Json<AgentRuntimeSpec>, _>("runtime_spec_snapshot")
            .map_err(database_error)?
            .0,
        command_snapshot: row
            .try_get::<Json<Vec<String>>, _>("command_snapshot")
            .map_err(database_error)?
            .0,
        bindings: ConnectionExecutionBindingSnapshot {
            artifact_trust_mode: row.try_get("artifact_trust_mode").map_err(database_error)?,
            bridge_image_digest: row.try_get("bridge_image_digest").map_err(database_error)?,
            mcp_gw_origin: row.try_get("mcp_gw_origin").map_err(database_error)?,
            mcp_gw_version: row.try_get("mcp_gw_version").map_err(database_error)?,
            namespace: row.try_get("runtime_namespace").map_err(database_error)?,
            runtime_class: row.try_get("runtime_class").map_err(database_error)?,
        },
        idempotency_identity: row
            .try_get("idempotency_identity")
            .map_err(database_error)?,
        uncached_status: row.try_get("uncached_status").map_err(database_error)?,
        operation_state: connection_operation_state_from_text(
            &row.try_get::<String, _>("operation_state")
                .map_err(database_error)?,
        )?,
        oauth_phase: connection_oauth_phase_from_text(
            &row.try_get::<String, _>("oauth_phase")
                .map_err(database_error)?,
        )?,
        authorization_url: row.try_get("authorization_url").map_err(database_error)?,
        authorization_url_digest: row
            .try_get("authorization_url_digest")
            .map_err(database_error)?,
        flow_expires_at: row
            .try_get("flow_expires_at_text")
            .map_err(database_error)?,
        cached_status: row
            .try_get::<Option<Json<serde_json::Value>>, _>("cached_status")
            .map_err(database_error)?
            .map(|value| value.0),
        result: row
            .try_get::<Option<Json<serde_json::Value>>, _>("result")
            .map_err(database_error)?
            .map(|value| value.0),
        failure_category: row.try_get("failure_category").map_err(database_error)?,
        finalization_state: row.try_get("finalization_state").map_err(database_error)?,
        cleanup_state: row.try_get("cleanup_state").map_err(database_error)?,
        cleanup_finding: row.try_get("cleanup_finding").map_err(database_error)?,
        response_deadline_at: row
            .try_get("response_deadline_at_text")
            .map_err(database_error)?,
        task_phase: task_phase_from_row(&row, "task_phase")?,
        runtime_uid: row.try_get("runtime_uid").map_err(database_error)?,
        output_archive: row.try_get("output_archive").map_err(database_error)?,
        finalize_requested: row.try_get("finalize_requested").map_err(database_error)?,
        finalized: row.try_get("finalized").map_err(database_error)?,
    })
}

fn task_record(row: sqlx::postgres::PgRow) -> Result<TaskRecord, StoreError> {
    let runtime_ownership = runtime_ownership_from_row(&row)?;
    let phase = task_phase_from_row(&row, "phase")?;
    Ok(TaskRecord {
        task_uid: row.try_get("task_uid").map_err(database_error)?,
        idempotency_key: row.try_get("idempotency_key").map_err(database_error)?,
        submitter_service: row.try_get("submitter_service").map_err(database_error)?,
        acting_user: row.try_get("acting_user").map_err(database_error)?,
        acting_user_id: row.try_get("acting_user_id").map_err(database_error)?,
        owner: row.try_get("owner").map_err(database_error)?,
        owner_user_id: row.try_get("owner_user_id").map_err(database_error)?,
        identity_binding_state: row
            .try_get("identity_binding_state")
            .map_err(database_error)?,
        workflow: row.try_get("workflow").map_err(database_error)?,
        workflow_name: row.try_get("workflow_name").map_err(database_error)?,
        workflow_version: row.try_get("workflow_version").map_err(database_error)?,
        workflow_digest: row.try_get("workflow_digest").map_err(database_error)?,
        user_envelope_instance_id: row
            .try_get("user_envelope_instance_id")
            .map_err(database_error)?,
        user_envelope_revision: row
            .try_get("user_envelope_revision")
            .map_err(database_error)?,
        user_envelope_digest: row
            .try_get("user_envelope_digest")
            .map_err(database_error)?,
        internal_authority_id: row
            .try_get("internal_authority_id")
            .map_err(database_error)?,
        internal_authority_version: row
            .try_get("internal_authority_version")
            .map_err(database_error)?,
        internal_authority_digest: row
            .try_get("internal_authority_digest")
            .map_err(database_error)?,
        coding_agent_runtime: row
            .try_get("coding_agent_runtime")
            .map_err(database_error)?,
        runtime_uid: row.try_get("runtime_uid").map_err(database_error)?,
        runtime_namespace: row.try_get("runtime_namespace").map_err(database_error)?,
        runtime_name: row.try_get("runtime_name").map_err(database_error)?,
        runtime_ownership,
        phase,
        runtime_spec: row
            .try_get::<Json<AgentRuntimeSpec>, _>("runtime_spec")
            .map_err(database_error)?
            .0,
        agent_command: row
            .try_get::<Json<Vec<String>>, _>("agent_command")
            .map_err(database_error)?
            .0,
        execution_binding: row
            .try_get::<Option<Json<TaskExecutionBinding>>, _>("execution_binding")
            .map_err(database_error)?
            .map(|binding| binding.0),
        input_archive: row.try_get("input_archive").map_err(database_error)?,
        output_archive: row.try_get("output_archive").map_err(database_error)?,
        execute_requested: row.try_get("execute_requested").map_err(database_error)?,
        finalize_requested: row.try_get("finalize_requested").map_err(database_error)?,
        finalized: row.try_get("finalized").map_err(database_error)?,
        failure_reason: row.try_get("failure_reason").map_err(database_error)?,
    })
}

const AGENT_RUN_SELECT: &str = "SELECT tasks.task_uid, tasks.submitter_service, tasks.acting_user, tasks.owner, tasks.owner_user_id, \
            tasks.workflow, tasks.workflow_name, tasks.workflow_version, tasks.workflow_digest, \
            tasks.user_envelope_instance_id, tasks.user_envelope_revision, tasks.user_envelope_digest, \
            tasks.coding_agent_runtime, tasks.runtime_uid, \
            tasks.runtime_ownership, tasks.phase, tasks.runtime_spec, \
            tasks.envelope_revision, tasks.finalize_requested, tasks.finalized, \
            tasks.failure_reason, \
            to_char(tasks.created_at AT TIME ZONE 'UTC', \
                    'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS created_at, \
            to_char(tasks.updated_at AT TIME ZONE 'UTC', \
                    'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS updated_at, \
            spend.observed_amount, spend.currency, spend.exhausted, spend.observed_at, \
            EXISTS ( \
                SELECT 1 FROM task_lifecycle_events history \
                WHERE history.task_uid = tasks.task_uid \
                  AND history.provenance = 'backfilled' \
            ) AS history_partial \
     FROM task_submissions tasks \
     LEFT JOIN LATERAL ( \
         SELECT observation.observed_amount::text AS observed_amount, \
                observation.currency, observation.exhausted, \
                to_char(observation.at AT TIME ZONE 'UTC', \
                        'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS observed_at \
         FROM spend_observations observation \
         WHERE observation.runtime_uid = tasks.runtime_uid \
         ORDER BY observation.at DESC, observation.id DESC \
         LIMIT 1 \
     ) spend ON true";

const ENVELOPE_REQUEST_COLUMNS: &str = "SELECT requests.id, requests.owner_user_id, requests.template_id, \
            requests.template_revision, requests.requested_envelope, \
            status.status, \
            CASE WHEN status.status = 'stale' THEN provisioned.approval_id ELSE status.approval_id END AS approval_id, \
            CASE WHEN status.status = 'stale' THEN provisioned.envelope_instance_id ELSE status.envelope_instance_id END AS envelope_instance_id, \
            CASE WHEN status.status = 'stale' THEN provisioned.envelope_digest ELSE status.envelope_digest END AS envelope_digest, \
            status.reason, \
            CASE WHEN status.status = 'stale' THEN provisioned.approved_envelope ELSE status.approved_envelope END AS approved_envelope, \
            status.actor AS status_actor, \
            status.template_revision AS status_template_revision, \
            to_char(requests.created_at AT TIME ZONE 'UTC', \
                    'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS created_at, \
            to_char(status.at AT TIME ZONE 'UTC', \
                    'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS status_at \
     FROM envelope_requests requests \
     JOIN LATERAL ( \
         SELECT events.status, events.approval_id, events.envelope_instance_id, \
                events.envelope_digest, events.reason, events.approved_envelope, \
                events.actor, events.template_revision, events.at \
         FROM envelope_request_events events \
         WHERE events.request_id = requests.id \
         ORDER BY events.at DESC, events.id DESC \
         LIMIT 1 \
     ) status ON true \
     LEFT JOIN LATERAL ( \
         SELECT events.approval_id, events.envelope_instance_id, \
                events.envelope_digest, events.approved_envelope \
         FROM envelope_request_events events \
         WHERE events.request_id = requests.id AND events.status = 'provisioned' \
         ORDER BY events.at DESC, events.id DESC \
         LIMIT 1 \
     ) provisioned ON status.status = 'stale' ";

fn agent_run_record(row: sqlx::postgres::PgRow) -> Result<AgentRunRecord, StoreError> {
    let observed_amount = row
        .try_get::<Option<String>, _>("observed_amount")
        .map_err(database_error)?;
    let spend = observed_amount
        .map(|observed_amount| {
            Ok(AgentRunSpend {
                observed_amount,
                currency: row.try_get("currency").map_err(database_error)?,
                exhausted: row.try_get("exhausted").map_err(database_error)?,
                observed_at: row.try_get("observed_at").map_err(database_error)?,
            })
        })
        .transpose()?;
    Ok(AgentRunRecord {
        task_uid: row.try_get("task_uid").map_err(database_error)?,
        submitter_service: row.try_get("submitter_service").map_err(database_error)?,
        acting_user: row.try_get("acting_user").map_err(database_error)?,
        owner: row.try_get("owner").map_err(database_error)?,
        owner_user_id: row.try_get("owner_user_id").map_err(database_error)?,
        workflow: row.try_get("workflow").map_err(database_error)?,
        workflow_name: row.try_get("workflow_name").map_err(database_error)?,
        workflow_version: row.try_get("workflow_version").map_err(database_error)?,
        workflow_digest: row.try_get("workflow_digest").map_err(database_error)?,
        user_envelope_instance_id: row
            .try_get("user_envelope_instance_id")
            .map_err(database_error)?,
        user_envelope_revision: row
            .try_get("user_envelope_revision")
            .map_err(database_error)?,
        user_envelope_digest: row
            .try_get("user_envelope_digest")
            .map_err(database_error)?,
        coding_agent_runtime: row
            .try_get("coding_agent_runtime")
            .map_err(database_error)?,
        runtime_uid: row.try_get("runtime_uid").map_err(database_error)?,
        runtime_ownership: runtime_ownership_from_row(&row)?,
        phase: task_phase_from_row(&row, "phase")?,
        runtime_spec: row
            .try_get::<Json<AgentRuntimeSpec>, _>("runtime_spec")
            .map_err(database_error)?
            .0,
        envelope_revision: row.try_get("envelope_revision").map_err(database_error)?,
        finalize_requested: row.try_get("finalize_requested").map_err(database_error)?,
        finalized: row.try_get("finalized").map_err(database_error)?,
        failure_reason: row.try_get("failure_reason").map_err(database_error)?,
        created_at: row.try_get("created_at").map_err(database_error)?,
        updated_at: row.try_get("updated_at").map_err(database_error)?,
        spend,
        history_partial: row.try_get("history_partial").map_err(database_error)?,
    })
}

fn agent_run_timeline_event(
    row: sqlx::postgres::PgRow,
) -> Result<AgentRunTimelineEvent, StoreError> {
    let kind = match row
        .try_get::<String, _>("event_kind")
        .map_err(database_error)?
        .as_str()
    {
        "phase" => AgentRunTimelineKind::Phase(task_phase_from_row(&row, "phase")?),
        "finalization_requested" => AgentRunTimelineKind::FinalizationRequested,
        "finalized" => AgentRunTimelineKind::Finalized,
        _ => return Err(StoreError::InvalidTaskTransition),
    };
    let provenance = match row
        .try_get::<String, _>("provenance")
        .map_err(database_error)?
        .as_str()
    {
        "recorded" => AgentRunTimelineProvenance::Recorded,
        "backfilled" => AgentRunTimelineProvenance::Backfilled,
        _ => return Err(StoreError::InvalidTaskTransition),
    };
    Ok(AgentRunTimelineEvent {
        kind,
        provenance,
        at: row.try_get("at").map_err(database_error)?,
    })
}

fn valid_workflow_publication(publication: &WorkflowPublication<'_>) -> bool {
    !publication.name.is_empty()
        && !publication.display_name.trim().is_empty()
        && !publication.agent.is_empty()
        && !publication.prompt.trim().is_empty()
        && !publication.content_digest.is_empty()
        && !publication.published_by.trim().is_empty()
}

fn workflow_revision_record(
    row: sqlx::postgres::PgRow,
) -> Result<WorkflowRevisionRecord, StoreError> {
    Ok(WorkflowRevisionRecord {
        name: row.try_get("name").map_err(database_error)?,
        version: row.try_get("version").map_err(database_error)?,
        display_name: row.try_get("display_name").map_err(database_error)?,
        agent: row.try_get("agent").map_err(database_error)?,
        prompt: row.try_get("prompt").map_err(database_error)?,
        content_digest: row.try_get("content_digest").map_err(database_error)?,
        published_by: row.try_get("published_by").map_err(database_error)?,
        published_at: row.try_get("published_at").map_err(database_error)?,
    })
}

fn envelope_request_record(
    row: sqlx::postgres::PgRow,
) -> Result<EnvelopeRequestRecord, StoreError> {
    let owner_user_id = row
        .try_get::<String, _>("owner_user_id")
        .map_err(database_error)
        .and_then(|value| {
            CanonicalUserId::parse(value).map_err(|_| StoreError::CanonicalIdentityInvalidRecord)
        })?;
    let status = envelope_request_status_from_text(
        &row.try_get::<String, _>("status").map_err(database_error)?,
    )?;
    Ok(EnvelopeRequestRecord {
        id: row.try_get("id").map_err(database_error)?,
        owner_user_id,
        template_id: row.try_get("template_id").map_err(database_error)?,
        template_revision: row.try_get("template_revision").map_err(database_error)?,
        requested_envelope: row
            .try_get::<Json<Envelope>, _>("requested_envelope")
            .map_err(database_error)?
            .0,
        approved_envelope: row
            .try_get::<Option<Json<Envelope>>, _>("approved_envelope")
            .map_err(database_error)?
            .map(|value| value.0),
        status,
        approval_id: row.try_get("approval_id").map_err(database_error)?,
        envelope_instance_id: row
            .try_get("envelope_instance_id")
            .map_err(database_error)?,
        envelope_digest: row.try_get("envelope_digest").map_err(database_error)?,
        reason: row.try_get("reason").map_err(database_error)?,
        status_actor: row.try_get("status_actor").map_err(database_error)?,
        status_template_revision: row
            .try_get("status_template_revision")
            .map_err(database_error)?,
        created_at: row.try_get("created_at").map_err(database_error)?,
        status_at: row.try_get("status_at").map_err(database_error)?,
    })
}

fn envelope_request_status_from_text(value: &str) -> Result<EnvelopeRequestStatus, StoreError> {
    match value {
        "pending" => Ok(EnvelopeRequestStatus::Pending),
        "approved" => Ok(EnvelopeRequestStatus::Approved),
        "rejected" => Ok(EnvelopeRequestStatus::Rejected),
        "provisioned" => Ok(EnvelopeRequestStatus::Provisioned),
        "stale" => Ok(EnvelopeRequestStatus::Stale),
        "conflict" => Ok(EnvelopeRequestStatus::Conflict),
        _ => Err(StoreError::InvalidEnvelopeRequestTransition),
    }
}

const fn valid_envelope_request_transition(
    from: EnvelopeRequestStatus,
    to: EnvelopeRequestStatus,
) -> bool {
    matches!(
        (from, to),
        (
            EnvelopeRequestStatus::Pending,
            EnvelopeRequestStatus::Approved
        ) | (
            EnvelopeRequestStatus::Pending,
            EnvelopeRequestStatus::Rejected
        ) | (
            EnvelopeRequestStatus::Pending,
            EnvelopeRequestStatus::Provisioned
        ) | (EnvelopeRequestStatus::Pending, EnvelopeRequestStatus::Stale)
            | (
                EnvelopeRequestStatus::Pending,
                EnvelopeRequestStatus::Conflict
            )
            | (
                EnvelopeRequestStatus::Approved,
                EnvelopeRequestStatus::Provisioned
            )
            | (
                EnvelopeRequestStatus::Approved,
                EnvelopeRequestStatus::Stale
            )
            | (
                EnvelopeRequestStatus::Approved,
                EnvelopeRequestStatus::Conflict
            )
            | (
                EnvelopeRequestStatus::Provisioned,
                EnvelopeRequestStatus::Stale
            )
    )
}

fn runtime_ownership_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<steward_types::RuntimeOwnership, StoreError> {
    match row
        .try_get::<String, _>("runtime_ownership")
        .map_err(database_error)?
        .as_str()
    {
        "provisioned" => Ok(steward_types::RuntimeOwnership::Provisioned),
        "adopted" => Ok(steward_types::RuntimeOwnership::Adopted),
        _ => Err(StoreError::InvalidTaskTransition),
    }
}

fn task_phase_from_row(
    row: &sqlx::postgres::PgRow,
    column: &str,
) -> Result<steward_types::TaskPhase, StoreError> {
    match row
        .try_get::<String, _>(column)
        .map_err(database_error)?
        .as_str()
    {
        "submitted" => Ok(steward_types::TaskPhase::Submitted),
        "parked" => Ok(steward_types::TaskPhase::Parked),
        "queued" => Ok(steward_types::TaskPhase::Queued),
        "running" => Ok(steward_types::TaskPhase::Running),
        "succeeded" => Ok(steward_types::TaskPhase::Succeeded),
        "failed" => Ok(steward_types::TaskPhase::Failed),
        "cancelled" => Ok(steward_types::TaskPhase::Cancelled),
        _ => Err(StoreError::InvalidTaskTransition),
    }
}

const fn ownership_text(ownership: steward_types::RuntimeOwnership) -> &'static str {
    match ownership {
        steward_types::RuntimeOwnership::Provisioned => "provisioned",
        steward_types::RuntimeOwnership::Adopted => "adopted",
    }
}

const fn task_phase_text(phase: steward_types::TaskPhase) -> &'static str {
    match phase {
        steward_types::TaskPhase::Submitted => "submitted",
        steward_types::TaskPhase::Parked => "parked",
        steward_types::TaskPhase::Queued => "queued",
        steward_types::TaskPhase::Running => "running",
        steward_types::TaskPhase::Succeeded => "succeeded",
        steward_types::TaskPhase::Failed => "failed",
        steward_types::TaskPhase::Cancelled => "cancelled",
    }
}

fn envelope_scope_kind(spec: &AgentRuntimeSpec) -> EnvelopeScopeKind {
    match &spec.principal {
        steward_types::Principal::User { .. } => EnvelopeScopeKind::MemberRole,
        steward_types::Principal::Service { .. } => EnvelopeScopeKind::Service,
    }
}

fn grant_expiry_error(error: sqlx::Error) -> StoreError {
    if error
        .as_database_error()
        .and_then(|error| error.code())
        .as_deref()
        == Some("22007")
    {
        StoreError::InvalidGrantExpiry
    } else {
        database_error(error)
    }
}

async fn lock_envelope_scope(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    scope_kind: EnvelopeScopeKind,
    scope_ref: &str,
) -> Result<(), StoreError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("{}:{scope_ref}", scope_kind.as_str()))
        .execute(&mut **transaction)
        .await
        .map_err(database_error)?;
    Ok(())
}

fn grant_dimension(delta: &AdmissionDelta) -> &'static str {
    match delta {
        AdmissionDelta::Budget { .. } => "budget",
        AdmissionDelta::SingleRunBudget { .. } => "budget-single-run",
        AdmissionDelta::Ttl { .. } => "ttl",
        AdmissionDelta::Models { .. } => "models",
        AdmissionDelta::Tools { .. } => "tools",
        AdmissionDelta::RunnerPlatforms { .. } => "runner-platforms",
        AdmissionDelta::RunnerMemory { .. } => "runner-memory",
        AdmissionDelta::RunnerCompute { .. } => "runner-compute",
        AdmissionDelta::RunnerStorage { .. } => "runner-storage",
    }
}
