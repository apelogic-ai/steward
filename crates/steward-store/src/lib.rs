//! Append-only operational history and approval-queue persistence.

use std::error::Error;
use std::fmt;

use sqlx::types::Json;
use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use steward_admission::{
    AdmissionDecision, AdmissionDelta, Envelope, EnvelopeScopeKind, EnvelopeSpec,
    envelope_is_within, evaluate,
};
use steward_types::{
    AgentRuntimeSpec, CanonicalPrincipal, CanonicalUserId, Email, OrganizationId,
    OrganizationIdentity, OrganizationIdentityMigration, TaskExecutionBinding,
};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskOrchestrationMode {
    Staged,
    Active,
}

impl TaskOrchestrationMode {
    pub fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "staged" => Ok(Self::Staged),
            "active" => Ok(Self::Active),
            _ => Err(StoreError::InvalidTaskTransition),
        }
    }

    pub fn is_active(self) -> bool {
        self == Self::Active
    }
}

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
            statement.push(" AND COALESCE(orchestration.runtime_uid, tasks.runtime_uid) = ");
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

    pub async fn service_envelope_revision(
        &self,
        service: &str,
        revision: i64,
    ) -> Result<Option<Envelope>, StoreError> {
        self.scoped_envelope_revision(EnvelopeScopeKind::Service, service, revision)
            .await
    }

    async fn scoped_envelope_revision(
        &self,
        scope_kind: EnvelopeScopeKind,
        scope_ref: &str,
        revision: i64,
    ) -> Result<Option<Envelope>, StoreError> {
        let row = sqlx::query(
            "SELECT revision, spec \
             FROM envelopes \
             WHERE scope_kind = $1 AND scope_ref = $2 AND revision = $3",
        )
        .bind(scope_kind.as_str())
        .bind(scope_ref)
        .bind(revision)
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
            .bind(request.task_uid.map_or_else(
                || {
                    format!(
                        "{}:{}:{}:{}:{}:{}:{}",
                        request.runtime_uid,
                        request.spec_digest,
                        request.envelope_revision,
                        request.base_spec_digest,
                        request.base_pending_approval_digest.unwrap_or_default(),
                        request.actor,
                        request.member_role,
                    )
                },
                |task_uid| format!("task-admission:{task_uid}"),
            ))
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
        let existing = sqlx::query(
            "SELECT \
                admission_decisions.id AS decision_id, \
                approvals.id AS approval_id, \
                approvals.decision_key, \
                approvals.evidence_url, \
                admission_decisions.runtime_uid, admission_decisions.runtime_namespace, \
                admission_decisions.runtime_name, admission_decisions.spec_digest, \
                admission_decisions.envelope_rev, admission_decisions.base_spec_digest, \
                admission_decisions.base_pending_approval_digest, \
                admission_decisions.base_spec, admission_decisions.deltas, \
                admission_decisions.proposed_spec, admission_decisions.actor, \
                admission_decisions.member_role \
             FROM admission_decisions \
             JOIN approvals ON approvals.admission_decision_id = admission_decisions.id \
             WHERE (($8::uuid IS NOT NULL AND admission_decisions.task_uid = $8) \
                    OR ($8::uuid IS NULL \
                        AND admission_decisions.runtime_uid = $1 \
                        AND admission_decisions.spec_digest = $2 \
                        AND admission_decisions.envelope_rev = $3 \
                        AND admission_decisions.base_spec_digest = $4 \
                        AND admission_decisions.actor = $5 \
                        AND admission_decisions.member_role = $6 \
                        AND admission_decisions.base_pending_approval_digest \
                            IS NOT DISTINCT FROM $7)) \
               AND ($8::uuid IS NOT NULL OR approvals.state = 'pending') \
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
        .bind(request.task_uid)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?;
        if let Some(row) = existing {
            let Json(base_spec) = row
                .try_get::<Json<AgentRuntimeSpec>, _>("base_spec")
                .map_err(database_error)?;
            let Json(deltas) = row
                .try_get::<Json<Vec<AdmissionDelta>>, _>("deltas")
                .map_err(database_error)?;
            let Json(proposed_spec) = row
                .try_get::<Json<AgentRuntimeSpec>, _>("proposed_spec")
                .map_err(database_error)?;
            if row
                .try_get::<String, _>("runtime_uid")
                .map_err(database_error)?
                != request.runtime_uid
                || row
                    .try_get::<String, _>("runtime_namespace")
                    .map_err(database_error)?
                    != request.runtime_namespace
                || row
                    .try_get::<String, _>("runtime_name")
                    .map_err(database_error)?
                    != request.runtime_name
                || row
                    .try_get::<String, _>("spec_digest")
                    .map_err(database_error)?
                    != request.spec_digest
                || row
                    .try_get::<i64, _>("envelope_rev")
                    .map_err(database_error)?
                    != request.envelope_revision
                || row
                    .try_get::<String, _>("base_spec_digest")
                    .map_err(database_error)?
                    != request.base_spec_digest
                || row
                    .try_get::<Option<String>, _>("base_pending_approval_digest")
                    .map_err(database_error)?
                    .as_deref()
                    != request.base_pending_approval_digest
                || base_spec != *request.base_spec
                || deltas != request.deltas
                || proposed_spec != *request.proposed_spec
                || row.try_get::<String, _>("actor").map_err(database_error)? != request.actor
                || row
                    .try_get::<String, _>("member_role")
                    .map_err(database_error)?
                    != request.member_role
            {
                return Err(StoreError::TaskIdempotencyConflict);
            }
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
              base_pending_approval_digest, task_uid) \
             VALUES ($1, $2, $3, $4, 'reject', $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
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
        .bind(request.task_uid)
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

    /// Recovers the first approval created for an exact server-authored Task admission.
    ///
    /// The Task may still be unbound after an ambiguous store failure, so this lookup uses the
    /// immutable admission identity rather than a Task runtime UID. Selecting the oldest match
    /// ensures retries recover the original approval even if an older server created a duplicate.
    pub async fn task_admission(
        &self,
        lookup: TaskAdmissionLookup,
    ) -> Result<Option<TaskAdmissionRecord>, StoreError> {
        let mut rows = sqlx::query(
            "SELECT admission_decisions.id AS decision_id, approvals.id AS approval_id, \
                    admission_decisions.runtime_uid, approvals.state, approvals.decision_key, \
                    approvals.evidence_url, admission_decisions.deltas, \
                    admission_decisions.proposed_spec \
             FROM admission_decisions \
             JOIN approvals ON approvals.admission_decision_id = admission_decisions.id \
             WHERE admission_decisions.task_uid = $1 \
             ORDER BY admission_decisions.at, admission_decisions.id, approvals.id \
             LIMIT 1",
        )
        .bind(lookup.task_uid)
        .fetch_all(&self.pool)
        .await
        .map_err(database_error)?;
        if rows.len() > 1 {
            return Err(StoreError::InvalidTaskTransition);
        }
        rows.pop().map(task_admission_record).transpose()
    }

    /// Returns a grant only when it is the exact, currently effective authority for this Task.
    /// Historical approval state alone never authorizes restoration or execution.
    pub async fn task_grant_application(
        &self,
        task_uid: Uuid,
        runtime_uid: &str,
    ) -> Result<Option<GrantApplication>, StoreError> {
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let (_, authority) = self
            .effective_task_authority(&mut transaction, task_uid, Some(runtime_uid))
            .await?;
        transaction.commit().await.map_err(database_error)?;
        Ok(match authority {
            EffectiveTaskAuthority::Active(application) => Some(*application),
            EffectiveTaskAuthority::Baseline { .. }
            | EffectiveTaskAuthority::Pending
            | EffectiveTaskAuthority::Inactive => None,
        })
    }

    async fn effective_task_authority(
        &self,
        transaction: &mut sqlx::Transaction<'_, Postgres>,
        task_uid: Uuid,
        expected_runtime_uid: Option<&str>,
    ) -> Result<(TaskRecord, EffectiveTaskAuthority), StoreError> {
        let task = sqlx::query("SELECT * FROM task_submissions WHERE task_uid = $1 FOR UPDATE")
            .bind(task_uid)
            .fetch_optional(&mut **transaction)
            .await
            .map_err(database_error)?
            .map(task_record)
            .transpose()?
            .ok_or(StoreError::TaskNotFound)?;
        lock_envelope_scope(
            transaction,
            EnvelopeScopeKind::Service,
            &task.submitter_service,
        )
        .await?;
        let latest_envelope = sqlx::query(
            "SELECT revision, spec FROM envelopes \
             WHERE scope_kind = 'service' AND scope_ref = $1 \
             ORDER BY revision DESC LIMIT 1",
        )
        .bind(&task.submitter_service)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(database_error)?;
        let latest_envelope = latest_envelope
            .map(|row| {
                let revision = row.try_get("revision").map_err(database_error)?;
                let Json(spec) = row
                    .try_get::<Json<EnvelopeSpec>, _>("spec")
                    .map_err(database_error)?;
                Ok::<_, StoreError>(Envelope { revision, spec })
            })
            .transpose()?;
        let internal_authority_pinned = task.internal_authority_id.is_some()
            && task.internal_authority_version.is_some()
            && task.internal_authority_digest.is_some();
        let current_envelope = latest_envelope
            .as_ref()
            .is_some_and(|envelope| envelope.revision == task.envelope_revision);
        let current_baseline_authority = internal_authority_pinned
            || latest_envelope.as_ref().is_some_and(|envelope| {
                matches!(
                    evaluate(&task.runtime_spec, envelope),
                    Ok(AdmissionDecision::Admit)
                )
            });

        let mut rows = sqlx::query(
            "SELECT admission_decisions.id AS decision_id, approvals.id AS approval_id, \
                    admission_decisions.runtime_uid, approvals.runtime_uid AS approval_runtime_uid, \
                    approvals.state, approvals.decision_key, approvals.evidence_url, \
                    admission_decisions.deltas, admission_decisions.proposed_spec, \
                    admission_decisions.base_spec, admission_decisions.spec_digest, \
                    admission_decisions.base_pending_approval_digest, \
                    admission_decisions.runtime_namespace, admission_decisions.runtime_name, \
                    admission_decisions.orchestration_operation_id, \
                    admission_decisions.envelope_rev, admission_decisions.actor, \
                    admission_decisions.member_role \
             FROM admission_decisions \
             JOIN approvals ON approvals.admission_decision_id = admission_decisions.id \
             WHERE admission_decisions.task_uid = $1 \
             ORDER BY approvals.id \
             LIMIT 2",
        )
        .bind(task_uid)
        .fetch_all(&mut **transaction)
        .await
        .map_err(database_error)?;
        if rows.is_empty() {
            let authority = if current_baseline_authority {
                EffectiveTaskAuthority::Baseline {
                    envelope_revision: if internal_authority_pinned {
                        task.envelope_revision
                    } else {
                        latest_envelope
                            .as_ref()
                            .map(|envelope| envelope.revision)
                            .ok_or(StoreError::StaleEnvelope)?
                    },
                }
            } else {
                EffectiveTaskAuthority::Inactive
            };
            return Ok((task, authority));
        }
        if rows.len() != 1 {
            return Ok((task, EffectiveTaskAuthority::Inactive));
        }
        let row = rows
            .pop()
            .ok_or_else(|| StoreError::Database("Task admission row disappeared".to_owned()))?;
        let approval_runtime_uid = row
            .try_get::<String, _>("approval_runtime_uid")
            .map_err(database_error)?;
        let runtime_namespace = row
            .try_get::<String, _>("runtime_namespace")
            .map_err(database_error)?;
        let runtime_name = row
            .try_get::<String, _>("runtime_name")
            .map_err(database_error)?;
        let envelope_revision = row
            .try_get::<i64, _>("envelope_rev")
            .map_err(database_error)?;
        let admission_operation_id = row
            .try_get::<Option<Uuid>, _>("orchestration_operation_id")
            .map_err(database_error)?;
        let actor = row.try_get::<String, _>("actor").map_err(database_error)?;
        let member_role = row
            .try_get::<String, _>("member_role")
            .map_err(database_error)?;
        let spec_digest = row
            .try_get::<String, _>("spec_digest")
            .map_err(database_error)?;
        let base_pending_approval_digest = row
            .try_get::<Option<String>, _>("base_pending_approval_digest")
            .map_err(database_error)?;
        let Json(base_spec) = row
            .try_get::<Json<AgentRuntimeSpec>, _>("base_spec")
            .map_err(database_error)?;
        let admission = task_admission_record(row)?;
        let exact_runtime = expected_runtime_uid
            .map(|runtime_uid| runtime_uid == admission.runtime_uid)
            .unwrap_or(true)
            && task
                .runtime_uid
                .as_deref()
                .map(|runtime_uid| runtime_uid == admission.runtime_uid)
                .unwrap_or(true)
            && approval_runtime_uid == admission.runtime_uid;
        let exact_admission = current_envelope
            && exact_runtime
            && task.runtime_ownership == steward_types::RuntimeOwnership::Provisioned
            && runtime_namespace == task.runtime_namespace
            && runtime_name == task.runtime_name
            && admission_operation_id == task.orchestration_operation_id
            && envelope_revision == task.envelope_revision
            && actor == task.submitter_service
            && member_role == task.submitter_service
            && admission.proposed_spec == task.runtime_spec
            && !admission.deltas.is_empty()
            && base_pending_approval_digest.as_deref() == Some(spec_digest.as_str());
        if !exact_admission {
            return Ok((task, EffectiveTaskAuthority::Inactive));
        }
        if admission.state == AdmissionApprovalState::Pending {
            return Ok((task, EffectiveTaskAuthority::Pending));
        }
        if admission.state != AdmissionApprovalState::Approved {
            return Ok((task, EffectiveTaskAuthority::Inactive));
        }

        let grants =
            sqlx::query("SELECT id FROM grants WHERE approval_id = $1 ORDER BY id FOR UPDATE")
                .bind(admission.approval_id)
                .fetch_all(&mut **transaction)
                .await
                .map_err(database_error)?;
        if grants.len() != admission.deltas.len() {
            return Ok((task, EffectiveTaskAuthority::Inactive));
        }
        let grant_status = sqlx::query(
            "SELECT count(grants.id)::bigint AS grant_count, \
                    COALESCE(bool_and( \
                        grants.runtime_uid = $2 \
                        AND grants.envelope_revision = $3 \
                        AND grants.expires_at > clock_timestamp() \
                        AND grant_revocations.grant_id IS NULL \
                    ), false) AS grants_active \
             FROM grants \
             LEFT JOIN grant_revocations ON grant_revocations.grant_id = grants.id \
             WHERE grants.approval_id = $1",
        )
        .bind(admission.approval_id)
        .bind(&admission.runtime_uid)
        .bind(envelope_revision)
        .fetch_one(&mut **transaction)
        .await
        .map_err(database_error)?;
        let grant_count = grant_status
            .try_get::<i64, _>("grant_count")
            .map_err(database_error)?;
        let grants_active = grant_status
            .try_get::<bool, _>("grants_active")
            .map_err(database_error)?;
        if !grants_active || grant_count != admission.deltas.len() as i64 {
            return Ok((task, EffectiveTaskAuthority::Inactive));
        }
        let application = GrantApplication {
            approval_id: admission.approval_id,
            application: GrantReversion {
                runtime_uid: admission.runtime_uid.clone(),
                runtime_namespace,
                runtime_name,
                actor,
                member_role,
                base_spec,
                proposed_spec: admission.proposed_spec.clone(),
                base_pending_approval_digest,
            },
        };
        Ok((task, EffectiveTaskAuthority::Active(Box::new(application))))
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
                admission_decisions.runtime_name, \
                admission_decisions.orchestration_operation_id \
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
            orchestration_operation_id: row
                .try_get("orchestration_operation_id")
                .map_err(database_error)?,
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
        validate_task_runtime_binding(request)?;
        validate_task_orchestration_reservation(request)?;
        let (phase, admission_text, deltas) = match request.admission_decision {
            AdmissionDecision::Admit => ("submitted", "admit", Vec::new()),
            AdmissionDecision::Reject { deltas } => ("parked", "reject", deltas.clone()),
        };
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        if let Some(record) = sqlx::query(
            "SELECT * FROM task_submissions \
             WHERE submitter_service = $1 AND owner_user_id = $2 \
               AND idempotency_key = $3 AND identity_binding_state = 'bound' \
             FOR UPDATE",
        )
        .bind(request.submitter_service)
        .bind(request.owner_user_id)
        .bind(request.idempotency_key)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?
        .map(task_record)
        .transpose()?
        {
            let operation = sqlx::query(TASK_RUNTIME_OPERATION_SELECT_BY_TASK)
                .bind(record.task_uid)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(database_error)?
                .map(task_runtime_operation_record)
                .transpose()?
                .ok_or(StoreError::InvalidTaskTransition)?;
            if !task_reservation_matches(&record, &operation, request, admission_text, &deltas) {
                return Err(StoreError::TaskIdempotencyConflict);
            }
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskReservation {
                inserted: false,
                record,
                operation,
            });
        }
        lock_envelope_scope(
            &mut transaction,
            EnvelopeScopeKind::Service,
            request.submitter_service,
        )
        .await?;
        let current_envelope = sqlx::query(
            "SELECT revision, spec FROM envelopes \
             WHERE scope_kind = 'service' AND scope_ref = $1 \
             ORDER BY revision DESC LIMIT 1",
        )
        .bind(request.submitter_service)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?
        .map(|row| {
            Ok::<_, StoreError>(Envelope {
                revision: row.try_get("revision").map_err(database_error)?,
                spec: row
                    .try_get::<Json<EnvelopeSpec>, _>("spec")
                    .map_err(database_error)?
                    .0,
            })
        })
        .transpose()?
        .ok_or(StoreError::StaleEnvelope)?;
        if current_envelope != *request.service_envelope
            || current_envelope.revision != request.envelope_revision
            || evaluate(request.runtime_spec, &current_envelope)
                .map_err(|_| StoreError::InvalidTaskTransition)?
                != *request.admission_decision
        {
            return Err(StoreError::StaleEnvelope);
        }
        let task_uid = request.task_uid;
        let operation_id = request.operation_id;
        let inserted = sqlx::query(
            "INSERT INTO task_submissions \
             (task_uid, idempotency_key, submitter_service, acting_user, acting_user_id, \
              owner, owner_user_id, identity_binding_state, workflow, \
              workflow_name, workflow_version, workflow_digest, \
              user_envelope_instance_id, user_envelope_revision, user_envelope_digest, \
              coding_agent_runtime, runtime_uid, runtime_namespace, runtime_name, runtime_ownership, phase, \
              runtime_spec, agent_command, execution_binding, envelope_revision, orchestration_version, \
              orchestration_operation_id, \
              candidate_digest, service_envelope_digest, original_admission_decision, \
              original_admission_deltas) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'bound', $8, $9, $10, $11, $12, $13, $14, \
                     $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, 2, $25, $26, $27, $28, $29) \
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
        .bind(Option::<&str>::None)
        .bind(request.runtime_namespace)
        .bind(request.runtime_name)
        .bind(ownership_text(request.runtime_ownership))
        .bind(phase)
        .bind(Json(request.runtime_spec))
        .bind(Json(request.agent_command))
        .bind(request.execution_binding.map(Json))
        .bind(request.envelope_revision)
        .bind(operation_id)
        .bind(request.candidate_digest)
        .bind(request.service_envelope_digest)
        .bind(admission_text)
        .bind(Json(&deltas))
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?
        .rows_affected()
            == 1;
        if inserted {
            let operation_ownership = task_runtime_ownership(request);
            sqlx::query(
                "INSERT INTO task_runtime_operations \
                 (task_uid, operation_id, state, generation, runtime_ownership, \
                  runtime_namespace, runtime_name, inert_manifest_digest, active_manifest_digest, \
                  expected_runtime_uid) \
                 VALUES ($1, $2, 'intent_recorded', 1, $3, $4, $5, $6, $7, $8)",
            )
            .bind(task_uid)
            .bind(operation_id)
            .bind(operation_ownership.as_str())
            .bind(request.runtime_namespace)
            .bind(request.runtime_name)
            .bind(request.inert_manifest_digest)
            .bind(request.active_manifest_digest)
            .bind(request.runtime_uid)
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
            sqlx::query(
                "INSERT INTO task_orchestration_journal \
                 (task_uid, operation_id, generation, state, event_kind, payload, actor) \
                 VALUES ($1, $2, 1, $3, 'task_reserved', '{}'::jsonb, $4)",
            )
            .bind(task_uid)
            .bind(operation_id)
            .bind(TaskOrchestrationState::IntentRecorded.as_str())
            .bind(request.submitter_service)
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
        }
        let record = sqlx::query(
            "SELECT * FROM task_submissions \
             WHERE submitter_service = $1 AND owner_user_id = $2 \
               AND idempotency_key = $3 AND identity_binding_state = 'bound'",
        )
        .bind(request.submitter_service)
        .bind(request.owner_user_id)
        .bind(request.idempotency_key)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?
        .map(task_record)
        .transpose()?
        .ok_or_else(|| {
            StoreError::Database("task reservation disappeared after idempotent insert".to_owned())
        })?;
        let operation = sqlx::query(TASK_RUNTIME_OPERATION_SELECT_BY_TASK)
            .bind(record.task_uid)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(database_error)?
            .map(task_runtime_operation_record)
            .transpose()?
            .ok_or(StoreError::InvalidTaskTransition)?;
        if !task_reservation_matches(&record, &operation, request, admission_text, &deltas) {
            return Err(StoreError::TaskIdempotencyConflict);
        }
        transaction.commit().await.map_err(database_error)?;
        Ok(TaskReservation {
            inserted,
            record,
            operation,
        })
    }

    pub async fn task_runtime_operation(
        &self,
        task_uid: Uuid,
    ) -> Result<Option<TaskRuntimeOperationRecord>, StoreError> {
        sqlx::query(TASK_RUNTIME_OPERATION_SELECT_BY_TASK)
            .bind(task_uid)
            .fetch_optional(&self.pool)
            .await
            .map_err(database_error)?
            .map(task_runtime_operation_record)
            .transpose()
    }

    pub async fn task_execution_attempt(
        &self,
        task_uid: Uuid,
    ) -> Result<Option<TaskExecutionAttemptRecord>, StoreError> {
        sqlx::query(TASK_EXECUTION_ATTEMPT_SELECT_BY_TASK)
            .bind(task_uid)
            .fetch_optional(&self.pool)
            .await
            .map_err(database_error)?
            .map(task_execution_attempt_record)
            .transpose()
    }

    pub async fn task_orchestration_work_items(
        &self,
    ) -> Result<Vec<TaskOrchestrationWorkItem>, StoreError> {
        let rows = sqlx::query(TASK_RUNTIME_OPERATION_SELECT_DUE)
            .fetch_all(&self.pool)
            .await
            .map_err(database_error)?;
        let mut work = Vec::with_capacity(rows.len());
        for row in rows {
            let operation = task_runtime_operation_record(row)?;
            let task = self
                .task(operation.task_uid)
                .await?
                .ok_or(StoreError::TaskNotFound)?;
            work.push(TaskOrchestrationWorkItem { task, operation });
        }
        Ok(work)
    }

    pub async fn task_execution_holds_runtime_lease(
        &self,
        attempt_id: Uuid,
    ) -> Result<bool, StoreError> {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM task_runtime_execution_leases WHERE attempt_id = $1)",
        )
        .bind(attempt_id)
        .fetch_one(&self.pool)
        .await
        .map_err(database_error)
    }

    pub async fn claim_approval_delivery(
        &self,
        worker: &str,
        lease_seconds: i64,
    ) -> Result<Option<ApprovalDeliveryWorkItem>, StoreError> {
        if worker.is_empty() || lease_seconds <= 0 || lease_seconds > 300 {
            return Err(StoreError::InvalidTaskTransition);
        }
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let claimed = sqlx::query(
            "WITH candidate AS ( \
                 SELECT id FROM external_effect_outbox \
                 WHERE effect_kind = 'approval_delivery' \
                   AND ( \
                     (state = 'pending' AND (retry_at IS NULL OR retry_at <= now())) \
                     OR (state = 'claimed' AND claimed_until <= now()) \
                   ) \
                 ORDER BY COALESCE(retry_at, created_at), id \
                 FOR UPDATE SKIP LOCKED LIMIT 1 \
             ) \
             UPDATE external_effect_outbox effects \
             SET state = 'claimed', generation = generation + 1, claimed_by = $1, \
                 claimed_until = now() + ($2::bigint * interval '1 second'), \
                 attempt_count = attempt_count + 1, retry_at = NULL, \
                 last_error_code = NULL, updated_at = now() \
             FROM candidate WHERE effects.id = candidate.id \
             RETURNING effects.id",
        )
        .bind(worker)
        .bind(lease_seconds)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?;
        let Some(claimed) = claimed else {
            transaction.commit().await.map_err(database_error)?;
            return Ok(None);
        };
        let effect_id = claimed.try_get::<Uuid, _>("id").map_err(database_error)?;
        let row = sqlx::query(
            "SELECT effects.id, effects.task_uid, effects.operation_id, effects.approval_id, \
                    effects.generation, effects.idempotency_key, approvals.runtime_uid, \
                    effects.delivery_invoked_at IS NOT NULL AS delivery_invoked, \
                    decisions.actor, decisions.member_role, decisions.deltas \
             FROM external_effect_outbox effects \
             JOIN approvals ON approvals.id = effects.approval_id \
             JOIN admission_decisions decisions \
               ON decisions.id = approvals.admission_decision_id \
             WHERE effects.id = $1 AND effects.state = 'claimed' \
               AND effects.claimed_by = $2",
        )
        .bind(effect_id)
        .bind(worker)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?
        .ok_or(StoreError::InvalidTaskTransition)?;
        let work = ApprovalDeliveryWorkItem {
            effect_id: row.try_get("id").map_err(database_error)?,
            task_uid: row.try_get("task_uid").map_err(database_error)?,
            operation_id: row.try_get("operation_id").map_err(database_error)?,
            approval_id: row.try_get("approval_id").map_err(database_error)?,
            generation: row.try_get("generation").map_err(database_error)?,
            delivery_invoked: row.try_get("delivery_invoked").map_err(database_error)?,
            idempotency_key: row.try_get("idempotency_key").map_err(database_error)?,
            runtime_uid: row.try_get("runtime_uid").map_err(database_error)?,
            actor: row.try_get("actor").map_err(database_error)?,
            member_role: row.try_get("member_role").map_err(database_error)?,
            deltas: row
                .try_get::<Json<Vec<AdmissionDelta>>, _>("deltas")
                .map_err(database_error)?
                .0,
        };
        transaction.commit().await.map_err(database_error)?;
        Ok(Some(work))
    }

    pub async fn authorize_approval_delivery_invocation(
        &self,
        work: &ApprovalDeliveryWorkItem,
        worker: &str,
    ) -> Result<Option<i64>, StoreError> {
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let task = task_in_transaction_for_update(&mut transaction, work.task_uid).await?;
        if task.finalized || task.finalize_requested || task.cancel_requested {
            transaction.commit().await.map_err(database_error)?;
            return Ok(None);
        }
        let generation = sqlx::query_scalar(
            "UPDATE external_effect_outbox SET delivery_invoked_at = now(), \
                 generation = generation + 1, updated_at = now() \
             WHERE id = $1 AND task_uid = $2 AND generation = $3 \
               AND state = 'claimed' AND claimed_by = $4 AND claimed_until > now() \
               AND delivery_invoked_at IS NULL RETURNING generation",
        )
        .bind(work.effect_id)
        .bind(work.task_uid)
        .bind(work.generation)
        .bind(worker)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?;
        transaction.commit().await.map_err(database_error)?;
        Ok(generation)
    }

    pub async fn complete_approval_delivery(
        &self,
        effect_id: Uuid,
        expected_generation: i64,
        worker: &str,
        decision_key: &str,
        evidence_url: &str,
    ) -> Result<ApprovalDeliveryTransition, StoreError> {
        if expected_generation <= 0
            || worker.is_empty()
            || decision_key.is_empty()
            || evidence_url.is_empty()
        {
            return Err(StoreError::InvalidTaskTransition);
        }
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let effect = sqlx::query(
            "SELECT approval_id, state, generation, external_reference, claimed_by \
             FROM external_effect_outbox WHERE id = $1 FOR UPDATE",
        )
        .bind(effect_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?
        .ok_or(StoreError::ApprovalNotFound)?;
        let approval_id = effect
            .try_get::<Uuid, _>("approval_id")
            .map_err(database_error)?;
        let state = effect
            .try_get::<String, _>("state")
            .map_err(database_error)?;
        let generation = effect
            .try_get::<i64, _>("generation")
            .map_err(database_error)?;
        if state == "delivered" {
            let approval =
                sqlx::query("SELECT decision_key, evidence_url FROM approvals WHERE id = $1")
                    .bind(approval_id)
                    .fetch_one(&mut *transaction)
                    .await
                    .map_err(database_error)?;
            let same = effect
                .try_get::<Option<String>, _>("external_reference")
                .map_err(database_error)?
                .as_deref()
                == Some(decision_key)
                && approval
                    .try_get::<Option<String>, _>("decision_key")
                    .map_err(database_error)?
                    .as_deref()
                    == Some(decision_key)
                && approval
                    .try_get::<Option<String>, _>("evidence_url")
                    .map_err(database_error)?
                    .as_deref()
                    == Some(evidence_url);
            transaction.commit().await.map_err(database_error)?;
            return Ok(if same {
                ApprovalDeliveryTransition::AlreadyApplied
            } else {
                ApprovalDeliveryTransition::Superseded
            });
        }
        let claimed_by = effect
            .try_get::<Option<String>, _>("claimed_by")
            .map_err(database_error)?;
        if state != "claimed"
            || generation != expected_generation
            || claimed_by.as_deref() != Some(worker)
        {
            transaction.commit().await.map_err(database_error)?;
            return Ok(ApprovalDeliveryTransition::Superseded);
        }
        let approval_updated = sqlx::query(
            "UPDATE approvals SET decision_key = $2, evidence_url = $3 \
             WHERE id = $1 AND ( \
               (decision_key IS NULL AND evidence_url IS NULL) \
               OR (decision_key = $2 AND evidence_url = $3) \
             )",
        )
        .bind(approval_id)
        .bind(decision_key)
        .bind(evidence_url)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        if approval_updated.rows_affected() != 1 {
            return Err(StoreError::InvalidTaskTransition);
        }
        let updated = sqlx::query(
            "UPDATE external_effect_outbox \
             SET state = 'delivered', generation = generation + 1, \
                 external_reference = $4, delivered_at = now(), claimed_by = NULL, \
                 claimed_until = NULL, updated_at = now() \
             WHERE id = $1 AND state = 'claimed' AND generation = $2 \
               AND claimed_by = $3",
        )
        .bind(effect_id)
        .bind(expected_generation)
        .bind(worker)
        .bind(decision_key)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        if updated.rows_affected() != 1 {
            return Err(StoreError::InvalidTaskTransition);
        }
        transaction.commit().await.map_err(database_error)?;
        Ok(ApprovalDeliveryTransition::Applied)
    }

    pub async fn retry_approval_delivery(
        &self,
        effect_id: Uuid,
        expected_generation: i64,
        worker: &str,
        error_code: &str,
    ) -> Result<ApprovalDeliveryTransition, StoreError> {
        if expected_generation <= 0 || worker.is_empty() || error_code.is_empty() {
            return Err(StoreError::InvalidTaskTransition);
        }
        let updated = sqlx::query(
            "UPDATE external_effect_outbox \
             SET state = 'pending', generation = generation + 1, claimed_by = NULL, \
                 claimed_until = NULL, retry_at = now() + interval '5 seconds', \
                 last_error_code = $4, updated_at = now() \
             WHERE id = $1 AND state = 'claimed' AND generation = $2 \
               AND claimed_by = $3",
        )
        .bind(effect_id)
        .bind(expected_generation)
        .bind(worker)
        .bind(error_code)
        .execute(&self.pool)
        .await
        .map_err(database_error)?;
        Ok(if updated.rows_affected() == 1 {
            ApprovalDeliveryTransition::Applied
        } else {
            ApprovalDeliveryTransition::Superseded
        })
    }

    pub async fn authorize_task_runtime_creation(
        &self,
        task_uid: Uuid,
        expected_generation: i64,
        actor: &str,
    ) -> Result<TaskOperationTransition, StoreError> {
        if expected_generation <= 0 || actor.is_empty() {
            return Err(StoreError::InvalidTaskTransition);
        }
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let updated = sqlx::query(
            "UPDATE task_runtime_operations operations \
             SET state = 'runtime_create_pending', generation = generation + 1, \
                 runtime_create_authorized_at = now(), \
                 retry_at = NULL, last_error_code = NULL, lease_owner = NULL, \
                 lease_expires_at = NULL, updated_at = now() \
             FROM task_submissions tasks \
             WHERE operations.task_uid = $1 AND operations.task_uid = tasks.task_uid \
               AND operations.state = 'intent_recorded' \
               AND operations.runtime_ownership = 'provisioned' \
               AND operations.generation = $2 AND NOT tasks.finalize_requested",
        )
        .bind(task_uid)
        .bind(expected_generation)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?
        .rows_affected()
            == 1;
        if updated {
            append_task_orchestration_journal(
                &mut transaction,
                task_uid,
                expected_generation + 1,
                TaskOrchestrationState::RuntimeCreatePending,
                "runtime_creation_authorized",
                actor,
            )
            .await?;
        }
        let current = task_runtime_operation_in_transaction(&mut transaction, task_uid).await?;
        transaction.commit().await.map_err(database_error)?;
        Ok(if updated {
            TaskOperationTransition::Applied(current)
        } else {
            TaskOperationTransition::Superseded(current)
        })
    }

    pub async fn record_task_runtime_observed(
        &self,
        task_uid: Uuid,
        expected_generation: i64,
        runtime_uid: &str,
        resource_version: &str,
        actor: &str,
    ) -> Result<TaskOperationTransition, StoreError> {
        if expected_generation <= 0
            || runtime_uid.is_empty()
            || resource_version.is_empty()
            || actor.is_empty()
        {
            return Err(StoreError::InvalidTaskTransition);
        }
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let updated = sqlx::query(
            "UPDATE task_runtime_operations operations \
             SET state = 'runtime_observed', generation = generation + 1, \
                 runtime_uid = $3, runtime_resource_version = $4, observed_at = now(), \
                 retry_at = NULL, last_error_code = NULL, lease_owner = NULL, \
                 lease_expires_at = NULL, updated_at = now() \
             FROM task_submissions tasks \
             WHERE operations.task_uid = $1 AND operations.task_uid = tasks.task_uid \
               AND ( \
                   (operations.state = 'runtime_create_pending' \
                    AND operations.runtime_ownership = 'provisioned') \
                   OR (operations.state = 'intent_recorded' \
                       AND operations.runtime_ownership IN ('adopted', 'resident') \
                       AND operations.expected_runtime_uid = $3) \
               ) \
               AND operations.generation = $2 AND operations.runtime_uid IS NULL \
               AND NOT tasks.finalized",
        )
        .bind(task_uid)
        .bind(expected_generation)
        .bind(runtime_uid)
        .bind(resource_version)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?
        .rows_affected()
            == 1;
        if updated {
            append_task_orchestration_journal(
                &mut transaction,
                task_uid,
                expected_generation + 1,
                TaskOrchestrationState::RuntimeObserved,
                "runtime_uid_observed",
                actor,
            )
            .await?;
        }
        let current = task_runtime_operation_in_transaction(&mut transaction, task_uid).await?;
        transaction.commit().await.map_err(database_error)?;
        Ok(if updated {
            TaskOperationTransition::Applied(current)
        } else {
            TaskOperationTransition::Superseded(current)
        })
    }

    pub async fn decide_task_runtime_authority(
        &self,
        task_uid: Uuid,
        expected_generation: i64,
        latest_envelope: &Envelope,
        latest_envelope_digest: &str,
        actor: &str,
    ) -> Result<TaskOperationTransition, StoreError> {
        if expected_generation <= 0
            || actor.is_empty()
            || !valid_sha256_reference(latest_envelope_digest)
        {
            return Err(StoreError::InvalidTaskTransition);
        }
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let task = task_in_transaction_for_update(&mut transaction, task_uid).await?;
        let current =
            task_runtime_operation_in_transaction_for_update(&mut transaction, task_uid).await?;
        if current.generation != expected_generation
            || !matches!(
                current.state,
                TaskOrchestrationState::RuntimeObserved | TaskOrchestrationState::ActivationPending
            )
        {
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskOperationTransition::Superseded(current));
        }
        let termination_requested = task.finalize_requested || task.cancel_requested;
        let internal_authority = task.internal_authority_id.is_some()
            && task.internal_authority_version.is_some()
            && task.internal_authority_digest.is_some();
        if internal_authority {
            if latest_envelope.revision != task.envelope_revision
                || task.service_envelope_digest.as_deref() != Some(latest_envelope_digest)
            {
                return Err(StoreError::StaleEnvelope);
            }
        } else {
            lock_envelope_scope(
                &mut transaction,
                EnvelopeScopeKind::Service,
                &task.submitter_service,
            )
            .await?;
            let persisted_latest = sqlx::query(
                "SELECT revision, spec FROM envelopes \
                 WHERE scope_kind = 'service' AND scope_ref = $1 \
                 ORDER BY revision DESC LIMIT 1",
            )
            .bind(&task.submitter_service)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(database_error)?
            .map(|row| {
                Ok::<_, StoreError>(Envelope {
                    revision: row.try_get("revision").map_err(database_error)?,
                    spec: row
                        .try_get::<Json<EnvelopeSpec>, _>("spec")
                        .map_err(database_error)?
                        .0,
                })
            })
            .transpose()?
            .ok_or(StoreError::StaleEnvelope)?;
            if persisted_latest != *latest_envelope {
                return Err(StoreError::StaleEnvelope);
            }
        }
        let decision = evaluate(&task.runtime_spec, latest_envelope)
            .map_err(|_| StoreError::InvalidTaskTransition)?;
        if current.state == TaskOrchestrationState::ActivationPending && !termination_requested {
            let authority_is_current = match current.activation_authority_kind.as_deref() {
                Some("internal") => internal_authority,
                Some("baseline") => !internal_authority && decision == AdmissionDecision::Admit,
                Some("grant") => {
                    let runtime_uid = current
                        .runtime_uid
                        .as_deref()
                        .ok_or(StoreError::InvalidTaskTransition)?;
                    let (_, effective) = self
                        .effective_task_authority(&mut transaction, task_uid, Some(runtime_uid))
                        .await?;
                    matches!(
                        effective,
                        EffectiveTaskAuthority::Active(application)
                            if current.approval_id == Some(application.approval_id)
                    )
                }
                _ => false,
            };
            if authority_is_current {
                let authority_kind = current
                    .activation_authority_kind
                    .as_deref()
                    .ok_or(StoreError::InvalidTaskTransition)?;
                let exact_authority_already_authorized =
                    current.activation_effect_authorized_at.is_some()
                        && current.activation_envelope_revision == Some(latest_envelope.revision)
                        && current.activation_envelope_digest.as_deref()
                            == Some(latest_envelope_digest);
                if exact_authority_already_authorized {
                    transaction.commit().await.map_err(database_error)?;
                    return Ok(TaskOperationTransition::AlreadyApplied(current));
                }
                let updated = sqlx::query(
                    "UPDATE task_runtime_operations \
                     SET generation = generation + 1, activation_authority_kind = $3, \
                         activation_envelope_revision = $4, activation_envelope_digest = $5, \
                         activation_effect_authorized_at = now(), retry_at = NULL, \
                         last_error_code = NULL, lease_owner = NULL, lease_expires_at = NULL, \
                         updated_at = now() \
                     WHERE task_uid = $1 AND generation = $2 AND state = 'activation_pending'",
                )
                .bind(task_uid)
                .bind(expected_generation)
                .bind(authority_kind)
                .bind(latest_envelope.revision)
                .bind(latest_envelope_digest)
                .execute(&mut *transaction)
                .await
                .map_err(database_error)?
                .rows_affected();
                if updated != 1 {
                    let current =
                        task_runtime_operation_in_transaction(&mut transaction, task_uid).await?;
                    transaction.commit().await.map_err(database_error)?;
                    return Ok(TaskOperationTransition::Superseded(current));
                }
                append_task_orchestration_journal(
                    &mut transaction,
                    task_uid,
                    expected_generation + 1,
                    TaskOrchestrationState::ActivationPending,
                    "activation_effect_authorized",
                    actor,
                )
                .await?;
                let current =
                    task_runtime_operation_in_transaction(&mut transaction, task_uid).await?;
                transaction.commit().await.map_err(database_error)?;
                return Ok(TaskOperationTransition::Applied(current));
            }
        }
        if current.state == TaskOrchestrationState::RuntimeObserved
            && !termination_requested
            && (internal_authority || decision == AdmissionDecision::Admit)
        {
            let authority_kind = if internal_authority {
                "internal"
            } else {
                "baseline"
            };
            let updated = sqlx::query(
                "UPDATE task_runtime_operations \
                 SET state = 'activation_pending', generation = generation + 1, \
                     activation_authority_kind = $3, activation_envelope_revision = $4, \
                     activation_envelope_digest = $5, retry_at = NULL, last_error_code = NULL, \
                     lease_owner = NULL, lease_expires_at = NULL, updated_at = now() \
                 WHERE task_uid = $1 AND generation = $2 AND state = 'runtime_observed'",
            )
            .bind(task_uid)
            .bind(expected_generation)
            .bind(authority_kind)
            .bind(latest_envelope.revision)
            .bind(latest_envelope_digest)
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?
            .rows_affected();
            if updated != 1 {
                let current =
                    task_runtime_operation_in_transaction(&mut transaction, task_uid).await?;
                transaction.commit().await.map_err(database_error)?;
                return Ok(TaskOperationTransition::Superseded(current));
            }
            append_task_orchestration_journal(
                &mut transaction,
                task_uid,
                expected_generation + 1,
                TaskOrchestrationState::ActivationPending,
                "activation_authority_selected",
                actor,
            )
            .await?;
            let current = task_runtime_operation_in_transaction(&mut transaction, task_uid).await?;
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskOperationTransition::Applied(current));
        }

        let approval_can_be_materialized = current.state == TaskOrchestrationState::RuntimeObserved
            && !termination_requested
            && task.original_admission_decision.as_deref() == Some("reject")
            && !task
                .original_admission_deltas
                .as_ref()
                .is_none_or(Vec::is_empty)
            && task.envelope_revision == latest_envelope.revision
            && task.service_envelope_digest.as_deref() == Some(latest_envelope_digest);
        if approval_can_be_materialized {
            let runtime_uid = current
                .runtime_uid
                .as_deref()
                .ok_or(StoreError::InvalidTaskTransition)?;
            let candidate_digest = task
                .candidate_digest
                .as_deref()
                .ok_or(StoreError::InvalidTaskTransition)?;
            let deltas = task
                .original_admission_deltas
                .as_ref()
                .ok_or(StoreError::InvalidTaskTransition)?;
            let approval_id = Uuid::new_v4();
            let decision_id = Uuid::new_v4();
            let outbox_id = Uuid::new_v4();
            let inert_spec = inert_task_runtime_spec(&task.runtime_spec, latest_envelope);
            sqlx::query(
                "INSERT INTO admission_decisions \
                 (id, runtime_uid, spec_digest, envelope_rev, verdict, deltas, proposed_spec, \
                  actor, member_role, base_spec_digest, base_spec, runtime_namespace, \
                  runtime_name, base_pending_approval_digest, task_uid, \
                  orchestration_operation_id) \
                 VALUES ($1, $2, $3, $4, 'reject', $5, $6, $7, $7, $8, $9, $10, $11, $3, $12, $13)",
            )
            .bind(decision_id)
            .bind(runtime_uid)
            .bind(candidate_digest)
            .bind(latest_envelope.revision)
            .bind(Json(deltas))
            .bind(Json(&task.runtime_spec))
            .bind(&task.submitter_service)
            .bind(&current.inert_manifest_digest)
            .bind(Json(&inert_spec))
            .bind(&current.runtime_namespace)
            .bind(&current.runtime_name)
            .bind(task_uid)
            .bind(current.operation_id)
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
            sqlx::query(
                "INSERT INTO approvals \
                 (id, runtime_uid, admission_decision_id, state) \
                 VALUES ($1, $2, $3, 'pending')",
            )
            .bind(approval_id)
            .bind(runtime_uid)
            .bind(decision_id)
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
            sqlx::query(
                "INSERT INTO external_effect_outbox \
                 (id, task_uid, operation_id, approval_id, effect_kind, idempotency_key, state) \
                 VALUES ($1, $2, $3, $4, 'approval_delivery', $5, 'pending')",
            )
            .bind(outbox_id)
            .bind(task_uid)
            .bind(current.operation_id)
            .bind(approval_id)
            .bind(format!("approval:{approval_id}"))
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
            sqlx::query(
                "UPDATE task_runtime_operations \
                 SET state = 'approval_pending', generation = generation + 1, \
                     approval_id = $3, retry_at = NULL, last_error_code = NULL, \
                     lease_owner = NULL, lease_expires_at = NULL, updated_at = now() \
                 WHERE task_uid = $1 AND generation = $2 AND state = 'runtime_observed'",
            )
            .bind(task_uid)
            .bind(expected_generation)
            .bind(approval_id)
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
            append_task_orchestration_journal(
                &mut transaction,
                task_uid,
                expected_generation + 1,
                TaskOrchestrationState::ApprovalPending,
                "runtime_bound_approval_materialized",
                actor,
            )
            .await?;
            let current = task_runtime_operation_in_transaction(&mut transaction, task_uid).await?;
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskOperationTransition::Applied(current));
        }

        sqlx::query(
            "UPDATE task_runtime_operations \
             SET state = 'cleanup_pending', generation = generation + 1, \
                 cleanup_requested_at = now(), last_error_code = 'authority_inactive', \
                 retry_at = NULL, lease_owner = NULL, lease_expires_at = NULL, updated_at = now() \
             WHERE task_uid = $1 AND generation = $2 AND state = $3",
        )
        .bind(task_uid)
        .bind(expected_generation)
        .bind(current.state.as_str())
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        sqlx::query(
            "UPDATE task_submissions \
             SET phase = CASE WHEN cancel_requested OR finalize_requested \
                     THEN 'cancelled' ELSE 'failed' END, \
                 finalize_requested = true, \
                 failure_reason = CASE WHEN cancel_requested OR finalize_requested \
                     THEN failure_reason \
                     ELSE COALESCE(failure_reason, 'admission_authority_inactive') END, \
                 updated_at = now() \
             WHERE task_uid = $1 AND NOT finalized",
        )
        .bind(task_uid)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        append_task_orchestration_journal(
            &mut transaction,
            task_uid,
            expected_generation + 1,
            TaskOrchestrationState::CleanupPending,
            if termination_requested {
                "termination_cleanup_requested"
            } else {
                "authority_inactive_cleanup_requested"
            },
            actor,
        )
        .await?;
        let current = task_runtime_operation_in_transaction(&mut transaction, task_uid).await?;
        transaction.commit().await.map_err(database_error)?;
        Ok(TaskOperationTransition::AuthorityInactive {
            current,
            reason: if termination_requested {
                "task_termination_requested"
            } else {
                "admission_authority_inactive"
            },
        })
    }

    pub async fn enter_task_cleanup(
        &self,
        task_uid: Uuid,
        expected_generation: i64,
        cause: TaskCleanupCause<'_>,
        actor: &str,
    ) -> Result<TaskOperationTransition, StoreError> {
        let (phase, failure_reason) = cause.task_outcome()?;
        if expected_generation <= 0 || actor.is_empty() {
            return Err(StoreError::InvalidTaskTransition);
        }
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let _task = task_in_transaction_for_update(&mut transaction, task_uid).await?;
        let current =
            task_runtime_operation_in_transaction_for_update(&mut transaction, task_uid).await?;
        if current.state == TaskOrchestrationState::Finalized
            || current.state == TaskOrchestrationState::CleanupPending
        {
            if current.state == TaskOrchestrationState::CleanupPending {
                fence_task_execution_for_cleanup(&mut transaction, task_uid).await?;
            }
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskOperationTransition::AlreadyApplied(current));
        }
        if current.generation != expected_generation {
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskOperationTransition::Superseded(current));
        }
        fence_task_execution_for_cleanup(&mut transaction, task_uid).await?;
        let updated = sqlx::query(
            "UPDATE task_runtime_operations \
             SET state = 'cleanup_pending', generation = generation + 1, \
                 cleanup_requested_at = now(), retry_at = NULL, last_error_code = NULL, \
                 lease_owner = NULL, lease_expires_at = NULL, updated_at = now() \
             WHERE task_uid = $1 AND generation = $2 AND state = $3",
        )
        .bind(task_uid)
        .bind(expected_generation)
        .bind(current.state.as_str())
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?
        .rows_affected();
        if updated != 1 {
            let current = task_runtime_operation_in_transaction(&mut transaction, task_uid).await?;
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskOperationTransition::Superseded(current));
        }
        sqlx::query(
            "UPDATE task_submissions \
             SET phase = CASE \
                     WHEN phase IN ('succeeded', 'failed', 'cancelled') THEN phase \
                     ELSE $2 \
                 END, \
                 failure_reason = COALESCE(failure_reason, $3), \
                 finalize_requested = true, updated_at = now() \
             WHERE task_uid = $1 AND orchestration_version = 2 AND NOT finalized",
        )
        .bind(task_uid)
        .bind(phase)
        .bind(failure_reason)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        append_task_orchestration_journal(
            &mut transaction,
            task_uid,
            expected_generation + 1,
            TaskOrchestrationState::CleanupPending,
            cause.event_kind(),
            actor,
        )
        .await?;
        let current = task_runtime_operation_in_transaction(&mut transaction, task_uid).await?;
        transaction.commit().await.map_err(database_error)?;
        Ok(TaskOperationTransition::Applied(current))
    }

    pub async fn record_task_activation_observed(
        &self,
        task_uid: Uuid,
        expected_generation: i64,
        observation: &TaskActivationObservation<'_>,
        actor: &str,
    ) -> Result<TaskOperationTransition, StoreError> {
        if expected_generation <= 0
            || observation.runtime_uid.is_empty()
            || observation.resource_version.is_empty()
            || !valid_sha256_reference(observation.active_manifest_digest)
            || actor.is_empty()
        {
            return Err(StoreError::InvalidTaskTransition);
        }
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let task = task_in_transaction_for_update(&mut transaction, task_uid).await?;
        let current =
            task_runtime_operation_in_transaction_for_update(&mut transaction, task_uid).await?;
        if current.generation != expected_generation
            || current.state != TaskOrchestrationState::ActivationPending
        {
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskOperationTransition::Superseded(current));
        }
        if current.runtime_uid.as_deref() != Some(observation.runtime_uid)
            || current.active_manifest_digest != observation.active_manifest_digest
        {
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskOperationTransition::InvariantViolation {
                current,
                reason: "active_runtime_identity_mismatch",
            });
        }
        let (_, authority) = self
            .effective_task_authority(&mut transaction, task_uid, Some(observation.runtime_uid))
            .await?;
        let authority_is_current = operation_authority_is_current(&authority, &current);
        if !authority_is_current
            || current.activation_effect_authorized_at.is_none()
            || task.finalize_requested
            || task.cancel_requested
        {
            sqlx::query(
                "UPDATE task_runtime_operations \
                 SET state = 'cleanup_pending', generation = generation + 1, \
                     cleanup_requested_at = now(), last_error_code = 'authority_inactive', \
                     retry_at = NULL, lease_owner = NULL, lease_expires_at = NULL, updated_at = now() \
                 WHERE task_uid = $1 AND generation = $2 AND state = 'activation_pending'",
            )
            .bind(task_uid)
            .bind(expected_generation)
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
            sqlx::query(
                "UPDATE task_submissions \
                 SET phase = CASE WHEN cancel_requested OR finalize_requested \
                         THEN 'cancelled' ELSE 'failed' END, \
                     finalize_requested = true, \
                     failure_reason = CASE WHEN cancel_requested OR finalize_requested \
                         THEN failure_reason \
                         ELSE COALESCE(failure_reason, 'admission_authority_inactive') END, \
                     updated_at = now() WHERE task_uid = $1 AND NOT finalized",
            )
            .bind(task_uid)
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
            append_task_orchestration_journal(
                &mut transaction,
                task_uid,
                expected_generation + 1,
                TaskOrchestrationState::CleanupPending,
                "activation_authority_inactive",
                actor,
            )
            .await?;
            let current = task_runtime_operation_in_transaction(&mut transaction, task_uid).await?;
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskOperationTransition::AuthorityInactive {
                current,
                reason: "admission_authority_inactive",
            });
        }
        if !observation.provider_set_ready {
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskOperationTransition::AlreadyApplied(current));
        }
        let updated = sqlx::query(
            "UPDATE task_runtime_operations \
             SET state = 'active', generation = generation + 1, \
                 runtime_resource_version = $3, activated_at = now(), \
                 retry_at = NULL, last_error_code = NULL, lease_owner = NULL, \
                 lease_expires_at = NULL, updated_at = now() \
             WHERE task_uid = $1 AND generation = $2 AND state = 'activation_pending' \
               AND runtime_uid = $4 AND active_manifest_digest = $5",
        )
        .bind(task_uid)
        .bind(expected_generation)
        .bind(observation.resource_version)
        .bind(observation.runtime_uid)
        .bind(observation.active_manifest_digest)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?
        .rows_affected();
        if updated != 1 {
            let current = task_runtime_operation_in_transaction(&mut transaction, task_uid).await?;
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskOperationTransition::Superseded(current));
        }
        sqlx::query(
            "UPDATE task_submissions \
             SET phase = CASE WHEN execute_requested THEN 'queued' ELSE 'submitted' END, \
                 updated_at = now() \
             WHERE task_uid = $1 AND phase IN ('submitted', 'parked', 'queued') \
               AND NOT finalize_requested AND NOT cancel_requested",
        )
        .bind(task_uid)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        append_task_orchestration_journal(
            &mut transaction,
            task_uid,
            expected_generation + 1,
            TaskOrchestrationState::Active,
            "active_manifest_observed",
            actor,
        )
        .await?;
        let current = task_runtime_operation_in_transaction(&mut transaction, task_uid).await?;
        transaction.commit().await.map_err(database_error)?;
        Ok(TaskOperationTransition::Applied(current))
    }

    pub async fn revalidate_active_task_authority(
        &self,
        task_uid: Uuid,
        expected_generation: i64,
        actor: &str,
    ) -> Result<TaskOperationTransition, StoreError> {
        if expected_generation <= 0 || actor.is_empty() {
            return Err(StoreError::InvalidTaskTransition);
        }
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let _task = task_in_transaction_for_update(&mut transaction, task_uid).await?;
        let current =
            task_runtime_operation_in_transaction_for_update(&mut transaction, task_uid).await?;
        if current.generation != expected_generation
            || current.state != TaskOrchestrationState::Active
        {
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskOperationTransition::Superseded(current));
        }
        let (task, authority) = self
            .effective_task_authority(&mut transaction, task_uid, current.runtime_uid.as_deref())
            .await?;
        if operation_authority_is_current(&authority, &current)
            && !task.finalize_requested
            && !task.cancel_requested
        {
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskOperationTransition::AlreadyApplied(current));
        }
        fence_task_execution_for_cleanup(&mut transaction, task_uid).await?;
        let updated = sqlx::query(
            "UPDATE task_runtime_operations \
             SET state = 'cleanup_pending', generation = generation + 1, \
                 cleanup_requested_at = now(), last_error_code = 'authority_inactive', \
                 retry_at = NULL, lease_owner = NULL, lease_expires_at = NULL, updated_at = now() \
             WHERE task_uid = $1 AND generation = $2 AND state = 'active'",
        )
        .bind(task_uid)
        .bind(expected_generation)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?
        .rows_affected();
        if updated != 1 {
            let current = task_runtime_operation_in_transaction(&mut transaction, task_uid).await?;
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskOperationTransition::Superseded(current));
        }
        sqlx::query(
            "UPDATE task_submissions \
             SET phase = CASE \
                     WHEN phase IN ('succeeded', 'failed', 'cancelled') THEN phase \
                     WHEN cancel_requested OR finalize_requested THEN 'cancelled' \
                     ELSE 'failed' END, \
                 finalize_requested = true, \
                 failure_reason = CASE \
                     WHEN phase IN ('succeeded', 'failed', 'cancelled') \
                         OR cancel_requested OR finalize_requested THEN failure_reason \
                     ELSE COALESCE(failure_reason, 'admission_authority_inactive') END, \
                 updated_at = now() WHERE task_uid = $1 AND NOT finalized",
        )
        .bind(task_uid)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        append_task_orchestration_journal(
            &mut transaction,
            task_uid,
            expected_generation + 1,
            TaskOrchestrationState::CleanupPending,
            "active_authority_inactive",
            actor,
        )
        .await?;
        let current = task_runtime_operation_in_transaction(&mut transaction, task_uid).await?;
        transaction.commit().await.map_err(database_error)?;
        Ok(TaskOperationTransition::AuthorityInactive {
            current,
            reason: "admission_authority_inactive",
        })
    }

    pub async fn authorize_task_activation_from_approval(
        &self,
        task_uid: Uuid,
        expected_generation: i64,
        actor: &str,
    ) -> Result<TaskOperationTransition, StoreError> {
        if expected_generation <= 0 || actor.is_empty() {
            return Err(StoreError::InvalidTaskTransition);
        }
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let task = task_in_transaction_for_update(&mut transaction, task_uid).await?;
        let current =
            task_runtime_operation_in_transaction_for_update(&mut transaction, task_uid).await?;
        if current.generation != expected_generation
            || current.state != TaskOrchestrationState::ApprovalPending
        {
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskOperationTransition::Superseded(current));
        }
        let runtime_uid = current
            .runtime_uid
            .as_deref()
            .ok_or(StoreError::InvalidTaskTransition)?;
        let (_, authority) = self
            .effective_task_authority(&mut transaction, task_uid, Some(runtime_uid))
            .await?;
        match authority {
            EffectiveTaskAuthority::Pending => {
                transaction.commit().await.map_err(database_error)?;
                Ok(TaskOperationTransition::AlreadyApplied(current))
            }
            EffectiveTaskAuthority::Active(application)
                if current.approval_id == Some(application.approval_id)
                    && !task.finalize_requested
                    && !task.cancel_requested =>
            {
                let envelope_digest = task
                    .service_envelope_digest
                    .as_deref()
                    .ok_or(StoreError::InvalidTaskTransition)?;
                let updated = sqlx::query(
                    "UPDATE task_runtime_operations \
                     SET state = 'activation_pending', generation = generation + 1, \
                         activation_authority_kind = 'grant', \
                         activation_envelope_revision = $3, \
                         activation_envelope_digest = $4, retry_at = NULL, \
                         last_error_code = NULL, lease_owner = NULL, lease_expires_at = NULL, \
                         updated_at = now() \
                     WHERE task_uid = $1 AND generation = $2 AND state = 'approval_pending'",
                )
                .bind(task_uid)
                .bind(expected_generation)
                .bind(task.envelope_revision)
                .bind(envelope_digest)
                .execute(&mut *transaction)
                .await
                .map_err(database_error)?
                .rows_affected();
                if updated != 1 {
                    let current =
                        task_runtime_operation_in_transaction(&mut transaction, task_uid).await?;
                    transaction.commit().await.map_err(database_error)?;
                    return Ok(TaskOperationTransition::Superseded(current));
                }
                append_task_orchestration_journal(
                    &mut transaction,
                    task_uid,
                    expected_generation + 1,
                    TaskOrchestrationState::ActivationPending,
                    "approved_authority_revalidated",
                    actor,
                )
                .await?;
                let current =
                    task_runtime_operation_in_transaction(&mut transaction, task_uid).await?;
                transaction.commit().await.map_err(database_error)?;
                Ok(TaskOperationTransition::Applied(current))
            }
            EffectiveTaskAuthority::Baseline { .. }
            | EffectiveTaskAuthority::Active(_)
            | EffectiveTaskAuthority::Inactive => {
                sqlx::query(
                    "UPDATE task_runtime_operations \
                     SET state = 'cleanup_pending', generation = generation + 1, \
                         cleanup_requested_at = now(), last_error_code = 'authority_inactive', \
                         retry_at = NULL, lease_owner = NULL, lease_expires_at = NULL, \
                         updated_at = now() \
                     WHERE task_uid = $1 AND generation = $2 AND state = 'approval_pending'",
                )
                .bind(task_uid)
                .bind(expected_generation)
                .execute(&mut *transaction)
                .await
                .map_err(database_error)?;
                sqlx::query(
                    "UPDATE task_submissions \
                     SET phase = 'failed', finalize_requested = true, \
                         failure_reason = COALESCE(failure_reason, 'admission_authority_inactive'), \
                         updated_at = now() WHERE task_uid = $1 AND NOT finalized",
                )
                .bind(task_uid)
                .execute(&mut *transaction)
                .await
                .map_err(database_error)?;
                append_task_orchestration_journal(
                    &mut transaction,
                    task_uid,
                    expected_generation + 1,
                    TaskOrchestrationState::CleanupPending,
                    "approval_authority_inactive",
                    actor,
                )
                .await?;
                let current =
                    task_runtime_operation_in_transaction(&mut transaction, task_uid).await?;
                transaction.commit().await.map_err(database_error)?;
                Ok(TaskOperationTransition::AuthorityInactive {
                    current,
                    reason: "admission_authority_inactive",
                })
            }
        }
    }

    pub async fn record_task_cleanup_complete(
        &self,
        task_uid: Uuid,
        expected_generation: i64,
        observation: TaskCleanupObservation,
        actor: &str,
    ) -> Result<TaskOperationTransition, StoreError> {
        if expected_generation <= 0 || actor.is_empty() {
            return Err(StoreError::InvalidTaskTransition);
        }
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let _task = task_in_transaction_for_update(&mut transaction, task_uid).await?;
        let current =
            task_runtime_operation_in_transaction_for_update(&mut transaction, task_uid).await?;
        if current.state == TaskOrchestrationState::Finalized {
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskOperationTransition::AlreadyApplied(current));
        }
        if current.generation != expected_generation
            || current.state != TaskOrchestrationState::CleanupPending
        {
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskOperationTransition::Superseded(current));
        }
        fence_task_execution_for_cleanup(&mut transaction, task_uid).await?;
        if let Some(attempt) =
            task_execution_attempt_for_task_in_transaction(&mut transaction, task_uid, true).await?
            && !attempt.state.is_terminal()
        {
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskOperationTransition::InvariantViolation {
                current,
                reason: "execution_cleanup_pending",
            });
        }
        if !retire_task_authority_for_cleanup(
            &mut transaction,
            task_uid,
            current.operation_id,
            actor,
        )
        .await?
        {
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskOperationTransition::InvariantViolation {
                current,
                reason: "approval_authority_cleanup_pending",
            });
        }
        let cleanup_is_complete = match (current.runtime_uid.as_ref(), current.runtime_ownership) {
            (Some(_), TaskRuntimeOwnership::Provisioned) => observation.exact_runtime_absent,
            (Some(_), TaskRuntimeOwnership::Adopted | TaskRuntimeOwnership::Resident) => true,
            (None, _) => current.runtime_create_authorized_at.is_none(),
        };
        if !cleanup_is_complete {
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskOperationTransition::InvariantViolation {
                current,
                reason: "cleanup_absence_not_proven",
            });
        }
        let updated = sqlx::query(
            "UPDATE task_runtime_operations \
             SET state = 'finalized', generation = generation + 1, \
                 runtime_absent_observed_at = CASE WHEN $3 THEN now() ELSE NULL END, \
                 projections_absent_observed_at = CASE WHEN $4 THEN now() ELSE NULL END, \
                 finalized_at = now(), retry_at = NULL, last_error_code = NULL, \
                 lease_owner = NULL, lease_expires_at = NULL, updated_at = now() \
             WHERE task_uid = $1 AND generation = $2 AND state = 'cleanup_pending'",
        )
        .bind(task_uid)
        .bind(expected_generation)
        .bind(observation.exact_runtime_absent)
        .bind(true)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?
        .rows_affected();
        if updated != 1 {
            let current = task_runtime_operation_in_transaction(&mut transaction, task_uid).await?;
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskOperationTransition::Superseded(current));
        }
        sqlx::query(
            "UPDATE task_submissions SET finalized = true, updated_at = now() \
             WHERE task_uid = $1 AND finalize_requested AND NOT finalized",
        )
        .bind(task_uid)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        append_task_orchestration_journal(
            &mut transaction,
            task_uid,
            expected_generation + 1,
            TaskOrchestrationState::Finalized,
            "cleanup_absence_observed",
            actor,
        )
        .await?;
        let current = task_runtime_operation_in_transaction(&mut transaction, task_uid).await?;
        transaction.commit().await.map_err(database_error)?;
        Ok(TaskOperationTransition::Applied(current))
    }

    /// Resolves an ambiguous create after cleanup has already been requested.
    /// This records the exact owned UID without reopening provisioning.
    pub async fn record_task_cleanup_runtime_observed(
        &self,
        task_uid: Uuid,
        expected_generation: i64,
        runtime_uid: &str,
        resource_version: &str,
        actor: &str,
    ) -> Result<TaskOperationTransition, StoreError> {
        if expected_generation <= 0
            || runtime_uid.is_empty()
            || resource_version.is_empty()
            || actor.is_empty()
        {
            return Err(StoreError::InvalidTaskTransition);
        }
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let _task = task_in_transaction_for_update(&mut transaction, task_uid).await?;
        let current =
            task_runtime_operation_in_transaction_for_update(&mut transaction, task_uid).await?;
        if current.generation != expected_generation
            || current.state != TaskOrchestrationState::CleanupPending
            || current.runtime_uid.is_some()
            || current.runtime_create_authorized_at.is_none()
            || current.runtime_ownership != TaskRuntimeOwnership::Provisioned
        {
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskOperationTransition::Superseded(current));
        }
        let updated = sqlx::query(
            "UPDATE task_runtime_operations \
             SET generation = generation + 1, runtime_uid = $3, \
                 runtime_resource_version = $4, observed_at = COALESCE(observed_at, now()), \
                 updated_at = now() \
             WHERE task_uid = $1 AND generation = $2 AND state = 'cleanup_pending' \
               AND runtime_uid IS NULL AND runtime_create_authorized_at IS NOT NULL",
        )
        .bind(task_uid)
        .bind(expected_generation)
        .bind(runtime_uid)
        .bind(resource_version)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?
        .rows_affected();
        if updated == 1 {
            append_task_orchestration_journal(
                &mut transaction,
                task_uid,
                expected_generation + 1,
                TaskOrchestrationState::CleanupPending,
                "ambiguous_runtime_uid_observed_for_cleanup",
                actor,
            )
            .await?;
        }
        let current = task_runtime_operation_in_transaction(&mut transaction, task_uid).await?;
        transaction.commit().await.map_err(database_error)?;
        Ok(if updated == 1 {
            TaskOperationTransition::Applied(current)
        } else {
            TaskOperationTransition::Superseded(current)
        })
    }

    pub async fn claim_task_execution_attempt(
        &self,
        task_uid: Uuid,
        command_digest: &str,
        input_digest: &str,
        actor: &str,
    ) -> Result<TaskExecutionTransition, StoreError> {
        if !valid_sha256_reference(command_digest)
            || !valid_sha256_reference(input_digest)
            || actor.is_empty()
        {
            return Err(StoreError::InvalidTaskTransition);
        }
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let task = task_in_transaction_for_update(&mut transaction, task_uid).await?;
        let operation =
            task_runtime_operation_in_transaction_for_update(&mut transaction, task_uid).await?;
        let existing =
            task_execution_attempt_for_task_in_transaction(&mut transaction, task_uid, true)
                .await?;
        if operation.state != TaskOrchestrationState::Active
            || !matches!(
                task.phase,
                steward_types::TaskPhase::Queued | steward_types::TaskPhase::Running
            )
            || !task.execute_requested
            || task.input_archive.is_none()
            || task.finalize_requested
            || task.cancel_requested
        {
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskExecutionTransition::InvariantViolation {
                attempt: existing,
                reason: "task_not_ready_for_execution",
            });
        }
        let runtime_uid = operation
            .runtime_uid
            .as_deref()
            .ok_or(StoreError::InvalidTaskTransition)?;
        let (_, authority) = self
            .effective_task_authority(&mut transaction, task_uid, Some(runtime_uid))
            .await?;
        let authority_is_current = operation_authority_is_current(&authority, &operation);
        if !authority_is_current {
            fence_task_execution_for_cleanup(&mut transaction, task_uid).await?;
            sqlx::query(
                "UPDATE task_runtime_operations \
                 SET state = 'cleanup_pending', generation = generation + 1, \
                     cleanup_requested_at = now(), last_error_code = 'authority_inactive', \
                     updated_at = now() \
                 WHERE task_uid = $1 AND generation = $2 AND state = 'active'",
            )
            .bind(task_uid)
            .bind(operation.generation)
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
            sqlx::query(
                "UPDATE task_submissions \
                 SET phase = 'failed', finalize_requested = true, \
                     failure_reason = COALESCE(failure_reason, 'admission_authority_inactive'), \
                     updated_at = now() WHERE task_uid = $1 AND NOT finalized",
            )
            .bind(task_uid)
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
            append_task_orchestration_journal(
                &mut transaction,
                task_uid,
                operation.generation + 1,
                TaskOrchestrationState::CleanupPending,
                "execution_authority_inactive",
                actor,
            )
            .await?;
            let existing =
                task_execution_attempt_for_task_in_transaction(&mut transaction, task_uid, false)
                    .await?;
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskExecutionTransition::AuthorityInactive {
                attempt: existing,
                reason: "admission_authority_inactive",
            });
        }
        if let Some(existing) = existing {
            let identity_matches = existing.operation_id == operation.operation_id
                && existing.runtime_uid == runtime_uid
                && existing.active_manifest_digest == operation.active_manifest_digest
                && existing.command_digest == command_digest
                && existing.input_digest == input_digest;
            transaction.commit().await.map_err(database_error)?;
            return Ok(if identity_matches {
                TaskExecutionTransition::AlreadyApplied(existing)
            } else {
                TaskExecutionTransition::InvariantViolation {
                    attempt: Some(existing),
                    reason: "execution_attempt_identity_conflict",
                }
            });
        }
        let attempt_id = Uuid::new_v4();
        let inserted = sqlx::query(
            "INSERT INTO task_runtime_execution_leases (runtime_uid, attempt_id) \
             VALUES ($1, $2) ON CONFLICT (runtime_uid) DO NOTHING",
        )
        .bind(runtime_uid)
        .bind(attempt_id)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?
        .rows_affected();
        if inserted == 0 {
            let lease_holder = sqlx::query(TASK_EXECUTION_ATTEMPT_SELECT_ACTIVE_BY_RUNTIME)
                .bind(runtime_uid)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(database_error)?
                .map(task_execution_attempt_record)
                .transpose()?;
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskExecutionTransition::InvariantViolation {
                attempt: lease_holder,
                reason: "runtime_execution_lease_held",
            });
        }
        sqlx::query(
            "INSERT INTO task_execution_attempts \
             (attempt_id, task_uid, operation_id, runtime_uid, active_manifest_digest, \
              command_digest, input_digest, state, generation) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'start_pending', 1)",
        )
        .bind(attempt_id)
        .bind(task_uid)
        .bind(operation.operation_id)
        .bind(runtime_uid)
        .bind(&operation.active_manifest_digest)
        .bind(command_digest)
        .bind(input_digest)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        let attempt = task_execution_attempt_in_transaction(&mut transaction, attempt_id, false)
            .await?
            .ok_or(StoreError::InvalidTaskTransition)?;
        transaction.commit().await.map_err(database_error)?;
        Ok(TaskExecutionTransition::Created(attempt))
    }

    /// Commits the single external start crossing before the adapter is invoked.
    /// A reconciler that later observes this timestamp must observe the adapter; it
    /// must never invoke start again.
    pub async fn authorize_task_execution_start(
        &self,
        attempt_id: Uuid,
        expected_generation: i64,
        actor: &str,
    ) -> Result<TaskExecutionTransition, StoreError> {
        if expected_generation <= 0 || actor.is_empty() {
            return Err(StoreError::InvalidTaskTransition);
        }
        let initial = sqlx::query(TASK_EXECUTION_ATTEMPT_SELECT_BY_ID)
            .bind(attempt_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(database_error)?
            .map(task_execution_attempt_record)
            .transpose()?
            .ok_or(StoreError::TaskNotFound)?;
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let task = task_in_transaction_for_update(&mut transaction, initial.task_uid).await?;
        let operation =
            task_runtime_operation_in_transaction_for_update(&mut transaction, initial.task_uid)
                .await?;
        let current = task_execution_attempt_in_transaction(&mut transaction, attempt_id, true)
            .await?
            .ok_or(StoreError::TaskNotFound)?;
        if current.generation != expected_generation
            || current.start_invoked_at.is_some()
            || current.state != TaskExecutionAttemptState::StartPending
        {
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskExecutionTransition::Superseded(current));
        }
        let identity_matches = operation.state == TaskOrchestrationState::Active
            && task.phase == steward_types::TaskPhase::Queued
            && task.execute_requested
            && !task.finalize_requested
            && !task.cancel_requested
            && operation.runtime_uid.as_deref() == Some(current.runtime_uid.as_str())
            && operation.operation_id == current.operation_id
            && operation.active_manifest_digest == current.active_manifest_digest;
        if !identity_matches {
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskExecutionTransition::InvariantViolation {
                attempt: Some(current),
                reason: "task_not_ready_for_execution_start",
            });
        }
        let (_, authority) = self
            .effective_task_authority(
                &mut transaction,
                initial.task_uid,
                Some(current.runtime_uid.as_str()),
            )
            .await?;
        let authority_is_current = operation_authority_is_current(&authority, &operation);
        if !authority_is_current {
            fence_task_execution_for_cleanup(&mut transaction, initial.task_uid).await?;
            sqlx::query(
                "UPDATE task_runtime_operations \
                 SET state = 'cleanup_pending', generation = generation + 1, \
                     cleanup_requested_at = now(), last_error_code = 'authority_inactive', \
                     updated_at = now() \
                 WHERE task_uid = $1 AND generation = $2 AND state = 'active'",
            )
            .bind(initial.task_uid)
            .bind(operation.generation)
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
            sqlx::query(
                "UPDATE task_submissions \
                 SET phase = 'failed', finalize_requested = true, \
                     failure_reason = COALESCE(failure_reason, 'admission_authority_inactive'), \
                     updated_at = now() WHERE task_uid = $1 AND NOT finalized",
            )
            .bind(initial.task_uid)
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;
            append_task_orchestration_journal(
                &mut transaction,
                initial.task_uid,
                operation.generation + 1,
                TaskOrchestrationState::CleanupPending,
                "execution_start_authority_inactive",
                actor,
            )
            .await?;
            let current =
                task_execution_attempt_in_transaction(&mut transaction, attempt_id, false)
                    .await?
                    .ok_or(StoreError::TaskNotFound)?;
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskExecutionTransition::AuthorityInactive {
                attempt: Some(current),
                reason: "admission_authority_inactive",
            });
        }
        let updated = sqlx::query(
            "UPDATE task_execution_attempts \
             SET generation = generation + 1, start_invoked_at = now(), \
                 start_observation_deadline_at = now() + interval '2 minutes', \
                 updated_at = now() \
             WHERE attempt_id = $1 AND generation = $2 AND state = 'start_pending' \
               AND start_invoked_at IS NULL",
        )
        .bind(attempt_id)
        .bind(expected_generation)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?
        .rows_affected();
        let current = task_execution_attempt_in_transaction(&mut transaction, attempt_id, false)
            .await?
            .ok_or(StoreError::TaskNotFound)?;
        transaction.commit().await.map_err(database_error)?;
        Ok(if updated == 1 {
            TaskExecutionTransition::Applied(current)
        } else {
            TaskExecutionTransition::Superseded(current)
        })
    }

    pub async fn record_task_execution_observation(
        &self,
        attempt_id: Uuid,
        expected_generation: i64,
        observation: TaskExecutionObservation<'_>,
        actor: &str,
    ) -> Result<TaskExecutionTransition, StoreError> {
        if expected_generation <= 0 || actor.is_empty() || !observation.is_valid() {
            return Err(StoreError::InvalidTaskTransition);
        }
        let initial = sqlx::query(TASK_EXECUTION_ATTEMPT_SELECT_BY_ID)
            .bind(attempt_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(database_error)?
            .map(task_execution_attempt_record)
            .transpose()?
            .ok_or(StoreError::TaskNotFound)?;
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let _task = task_in_transaction_for_update(&mut transaction, initial.task_uid).await?;
        let operation =
            task_runtime_operation_in_transaction_for_update(&mut transaction, initial.task_uid)
                .await?;
        let current = task_execution_attempt_in_transaction(&mut transaction, attempt_id, true)
            .await?
            .ok_or(StoreError::TaskNotFound)?;
        if current.state.is_terminal() {
            if current.state == TaskExecutionAttemptState::OutcomeUnknown {
                record_terminal_execution_retirement(&mut transaction, &current, observation)
                    .await?;
            }
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskExecutionTransition::AlreadyApplied(current));
        }
        if current.generation != expected_generation {
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskExecutionTransition::Superseded(current));
        }
        let next_generation = expected_generation + 1;
        match observation {
            TaskExecutionObservation::Accepted {
                adapter_observation_id,
            }
            | TaskExecutionObservation::Running {
                adapter_observation_id,
            } => {
                sqlx::query(
                    "UPDATE task_execution_attempts \
                     SET state = 'running', generation = generation + 1, \
                         adapter_observation_id = $3, started_at = COALESCE(started_at, now()), \
                         retry_at = NULL, last_error_code = NULL, updated_at = now() \
                     WHERE attempt_id = $1 AND generation = $2 \
                       AND state IN ('start_pending', 'running')",
                )
                .bind(attempt_id)
                .bind(expected_generation)
                .bind(adapter_observation_id)
                .execute(&mut *transaction)
                .await
                .map_err(database_error)?;
                sqlx::query(
                    "UPDATE task_submissions SET phase = 'running', updated_at = now() \
                     WHERE task_uid = $1 AND phase = 'queued' AND NOT finalize_requested \
                       AND NOT cancel_requested",
                )
                .bind(current.task_uid)
                .execute(&mut *transaction)
                .await
                .map_err(database_error)?;
            }
            TaskExecutionObservation::Succeeded {
                adapter_observation_id,
                result_digest,
                result_reference,
                output_archive,
            } => {
                sqlx::query(
                    "UPDATE task_submissions SET phase = 'running', updated_at = now() \
                     WHERE task_uid = $1 AND phase = 'queued' AND NOT finalize_requested \
                       AND NOT cancel_requested",
                )
                .bind(current.task_uid)
                .execute(&mut *transaction)
                .await
                .map_err(database_error)?;
                sqlx::query(
                    "UPDATE task_execution_attempts \
                     SET state = 'succeeded', generation = generation + 1, \
                         adapter_observation_id = $3, result_digest = $4, result_reference = $5, \
                         started_at = COALESCE(started_at, now()), finished_at = now(), \
                         retry_at = NULL, last_error_code = NULL, updated_at = now() \
                     WHERE attempt_id = $1 AND generation = $2 \
                       AND state IN ('start_pending', 'running', 'cancel_pending')",
                )
                .bind(attempt_id)
                .bind(expected_generation)
                .bind(adapter_observation_id)
                .bind(result_digest)
                .bind(result_reference)
                .execute(&mut *transaction)
                .await
                .map_err(database_error)?;
                sqlx::query(
                    "UPDATE task_submissions \
                     SET phase = 'succeeded', output_archive = $2, updated_at = now() \
                     WHERE task_uid = $1 AND phase IN ('queued', 'running') \
                       AND NOT finalize_requested AND NOT cancel_requested",
                )
                .bind(current.task_uid)
                .bind(output_archive)
                .execute(&mut *transaction)
                .await
                .map_err(database_error)?;
            }
            TaskExecutionObservation::Failed {
                adapter_observation_id,
                reason,
            } => {
                sqlx::query(
                    "UPDATE task_execution_attempts \
                     SET state = 'failed', generation = generation + 1, \
                         adapter_observation_id = $3, last_error_code = $4, \
                         started_at = COALESCE(started_at, now()), finished_at = now(), \
                         retry_at = NULL, updated_at = now() \
                     WHERE attempt_id = $1 AND generation = $2 \
                       AND state IN ('start_pending', 'running', 'cancel_pending')",
                )
                .bind(attempt_id)
                .bind(expected_generation)
                .bind(adapter_observation_id)
                .bind(reason)
                .execute(&mut *transaction)
                .await
                .map_err(database_error)?;
                if operation.state != TaskOrchestrationState::CleanupPending {
                    sqlx::query(
                        "UPDATE task_runtime_operations \
                         SET state = 'cleanup_pending', generation = generation + 1, \
                             cleanup_requested_at = now(), \
                             last_error_code = 'execution_failed', retry_at = NULL, \
                             lease_owner = NULL, lease_expires_at = NULL, updated_at = now() \
                         WHERE task_uid = $1 AND generation = $2 AND state <> 'finalized'",
                    )
                    .bind(current.task_uid)
                    .bind(operation.generation)
                    .execute(&mut *transaction)
                    .await
                    .map_err(database_error)?;
                    append_task_orchestration_journal(
                        &mut transaction,
                        current.task_uid,
                        operation.generation + 1,
                        TaskOrchestrationState::CleanupPending,
                        "execution_failed_cleanup_requested",
                        actor,
                    )
                    .await?;
                }
                sqlx::query(
                    "UPDATE task_submissions \
                     SET phase = 'failed', failure_reason = COALESCE(failure_reason, $2), \
                         finalize_requested = true, updated_at = now() \
                     WHERE task_uid = $1 AND phase IN ('queued', 'running')",
                )
                .bind(current.task_uid)
                .bind(reason)
                .execute(&mut *transaction)
                .await
                .map_err(database_error)?;
            }
            TaskExecutionObservation::OutcomeUnknown { reason } => {
                sqlx::query(
                    "UPDATE task_execution_attempts \
                     SET state = 'outcome_unknown', generation = generation + 1, \
                         last_error_code = $3, finished_at = now(), retry_at = NULL, \
                         updated_at = now() \
                     WHERE attempt_id = $1 AND generation = $2 \
                       AND state IN ('start_pending', 'running', 'cancel_pending')",
                )
                .bind(attempt_id)
                .bind(expected_generation)
                .bind(reason)
                .execute(&mut *transaction)
                .await
                .map_err(database_error)?;
                if operation.state != TaskOrchestrationState::CleanupPending {
                    sqlx::query(
                        "UPDATE task_runtime_operations \
                         SET state = 'cleanup_pending', generation = generation + 1, \
                             cleanup_requested_at = now(), \
                             last_error_code = 'execution_outcome_unknown', retry_at = NULL, \
                             lease_owner = NULL, lease_expires_at = NULL, updated_at = now() \
                         WHERE task_uid = $1 AND generation = $2 AND state <> 'finalized'",
                    )
                    .bind(current.task_uid)
                    .bind(operation.generation)
                    .execute(&mut *transaction)
                    .await
                    .map_err(database_error)?;
                    append_task_orchestration_journal(
                        &mut transaction,
                        current.task_uid,
                        operation.generation + 1,
                        TaskOrchestrationState::CleanupPending,
                        "execution_outcome_unknown_cleanup_requested",
                        actor,
                    )
                    .await?;
                }
                sqlx::query(
                    "UPDATE task_submissions \
                     SET phase = CASE \
                             WHEN phase IN ('succeeded', 'failed', 'cancelled') THEN phase \
                             ELSE 'failed' END, \
                         finalize_requested = true, \
                         failure_reason = CASE \
                             WHEN phase IN ('succeeded', 'failed', 'cancelled') THEN failure_reason \
                             ELSE COALESCE(failure_reason, 'execution_outcome_unknown') END, \
                         updated_at = now() WHERE task_uid = $1 AND NOT finalized",
                )
                .bind(current.task_uid)
                .execute(&mut *transaction)
                .await
                .map_err(database_error)?;
            }
        }
        let updated = task_execution_attempt_in_transaction(&mut transaction, attempt_id, false)
            .await?
            .ok_or(StoreError::TaskNotFound)?;
        if updated.generation != next_generation {
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskExecutionTransition::Superseded(updated));
        }
        record_terminal_execution_retirement(&mut transaction, &updated, observation).await?;
        transaction.commit().await.map_err(database_error)?;
        Ok(TaskExecutionTransition::Applied(updated))
    }

    pub async fn authorize_task_execution_cancel(
        &self,
        attempt_id: Uuid,
        expected_generation: i64,
        actor: &str,
    ) -> Result<TaskExecutionTransition, StoreError> {
        if expected_generation <= 0 || actor.is_empty() {
            return Err(StoreError::InvalidTaskTransition);
        }
        let initial = sqlx::query(TASK_EXECUTION_ATTEMPT_SELECT_BY_ID)
            .bind(attempt_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(database_error)?
            .map(task_execution_attempt_record)
            .transpose()?
            .ok_or(StoreError::TaskNotFound)?;
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let task = task_in_transaction_for_update(&mut transaction, initial.task_uid).await?;
        let current = task_execution_attempt_in_transaction(&mut transaction, attempt_id, true)
            .await?
            .ok_or(StoreError::TaskNotFound)?;
        if current.generation != expected_generation {
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskExecutionTransition::Superseded(current));
        }
        if current.state == TaskExecutionAttemptState::CancelPending {
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskExecutionTransition::AlreadyApplied(current));
        }
        if !(task.cancel_requested || task.finalize_requested)
            || current.start_invoked_at.is_none()
            || !matches!(
                current.state,
                TaskExecutionAttemptState::StartPending | TaskExecutionAttemptState::Running
            )
        {
            transaction.commit().await.map_err(database_error)?;
            return Ok(TaskExecutionTransition::InvariantViolation {
                attempt: Some(current),
                reason: "execution_not_cancellable",
            });
        }
        let updated = sqlx::query(
            "UPDATE task_execution_attempts \
             SET state = 'cancel_pending', generation = generation + 1, updated_at = now() \
             WHERE attempt_id = $1 AND generation = $2 \
               AND state IN ('start_pending', 'running')",
        )
        .bind(attempt_id)
        .bind(expected_generation)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?
        .rows_affected();
        let current = task_execution_attempt_in_transaction(&mut transaction, attempt_id, false)
            .await?
            .ok_or(StoreError::TaskNotFound)?;
        transaction.commit().await.map_err(database_error)?;
        Ok(if updated == 1 {
            TaskExecutionTransition::Applied(current)
        } else {
            TaskExecutionTransition::Superseded(current)
        })
    }

    pub async fn task_execution_start_observation_expired(
        &self,
        attempt_id: Uuid,
    ) -> Result<bool, StoreError> {
        sqlx::query_scalar(
            "SELECT COALESCE(start_observation_deadline_at <= now(), false) \
             FROM task_execution_attempts WHERE attempt_id = $1",
        )
        .bind(attempt_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(database_error)?
        .ok_or(StoreError::TaskNotFound)
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
                        tasks.phase AS task_phase, COALESCE((SELECT runtime_uid FROM task_runtime_operations runtime_operation WHERE runtime_operation.task_uid = tasks.task_uid), tasks.runtime_uid) AS runtime_uid, \
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
                        tasks.phase AS task_phase, COALESCE((SELECT runtime_uid FROM task_runtime_operations runtime_operation WHERE runtime_operation.task_uid = tasks.task_uid), tasks.runtime_uid) AS runtime_uid, \
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
                            tasks.phase AS task_phase, COALESCE((SELECT runtime_uid FROM task_runtime_operations runtime_operation WHERE runtime_operation.task_uid = tasks.task_uid), tasks.runtime_uid) AS runtime_uid, \
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
              internal_authority_id, internal_authority_version, internal_authority_digest, \
              orchestration_version, orchestration_operation_id, candidate_digest, service_envelope_digest, \
              original_admission_decision, original_admission_deltas) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'bound', $8, $9, $10, $11, \
                     'provisioned', 'queued', $12, $13, $14, true, $15, $16, $17, $18, \
                     2, $1, $19, $20, 'admit', '[]'::jsonb)",
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
        .bind(task.candidate_digest)
        .bind(task.service_envelope_digest)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        sqlx::query(
            "INSERT INTO task_runtime_operations \
             (task_uid, operation_id, state, generation, runtime_ownership, runtime_namespace, \
              runtime_name, inert_manifest_digest, active_manifest_digest) \
             VALUES ($1, $1, 'intent_recorded', 1, 'provisioned', $2, $3, $4, $5)",
        )
        .bind(request.operation_id)
        .bind(task.runtime_namespace)
        .bind(task.runtime_name)
        .bind(task.inert_manifest_digest)
        .bind(task.active_manifest_digest)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        sqlx::query(
            "INSERT INTO task_orchestration_journal \
             (task_uid, operation_id, generation, state, event_kind, payload, actor) \
             VALUES ($1, $1, 1, 'intent_recorded', 'internal_task_reserved', '{}'::jsonb, $2)",
        )
        .bind(request.operation_id)
        .bind(task.submitter_service)
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
                    tasks.phase AS task_phase, COALESCE((SELECT runtime_uid FROM task_runtime_operations runtime_operation WHERE runtime_operation.task_uid = tasks.task_uid), tasks.runtime_uid) AS runtime_uid, \
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
                    tasks.phase AS task_phase, COALESCE((SELECT runtime_uid FROM task_runtime_operations runtime_operation WHERE runtime_operation.task_uid = tasks.task_uid), tasks.runtime_uid) AS runtime_uid, \
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

    /// Internal validating-webhook lookup for a connection runtime transition. Namespace and
    /// name come from the admission object and must resolve to exactly one live, server-authored
    /// operation. The returned orchestration projection lets admission distinguish the inert
    /// CREATE from the later active UPDATE without trusting Kubernetes object annotations.
    pub async fn connection_runtime_admission(
        &self,
        runtime_namespace: &str,
        runtime_name: &str,
    ) -> Result<Option<ConnectionRuntimeAdmissionRecord>, StoreError> {
        let rows = sqlx::query(
            "SELECT operations.*, \
                    to_char(operations.flow_expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS flow_expires_at_text, \
                    to_char(operations.response_deadline_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS response_deadline_at_text, \
                    tasks.phase AS task_phase, runtime_operation.runtime_uid, \
                    tasks.output_archive, tasks.finalize_requested, tasks.finalized, \
                    tasks.candidate_digest, runtime_operation.state AS orchestration_state, \
                    runtime_operation.runtime_ownership AS orchestration_runtime_ownership, \
                    runtime_operation.inert_manifest_digest, \
                    runtime_operation.active_manifest_digest, \
                    runtime_operation.runtime_create_authorized_at IS NOT NULL \
                        AS runtime_create_authorized, \
                    runtime_operation.activation_effect_authorized_at IS NOT NULL \
                        AS activation_effect_authorized \
             FROM connection_operations operations \
             JOIN task_submissions tasks ON tasks.task_uid = operations.task_uid \
             JOIN task_runtime_operations runtime_operation \
               ON runtime_operation.task_uid = tasks.task_uid \
             WHERE runtime_operation.runtime_namespace = $1 \
               AND runtime_operation.runtime_name = $2 \
               AND runtime_operation.state IN ('runtime_create_pending', 'activation_pending') \
               AND runtime_operation.runtime_ownership = 'provisioned' \
               AND operations.operation_state = 'queued' \
               AND operations.finalization_state = 'not_requested' \
               AND tasks.phase = 'queued' \
               AND NOT tasks.cancel_requested \
               AND NOT tasks.finalize_requested \
               AND NOT tasks.finalized \
               AND operations.response_deadline_at > now() \
             ORDER BY operations.created_at DESC LIMIT 2",
        )
        .bind(runtime_namespace)
        .bind(runtime_name)
        .fetch_all(&self.pool)
        .await
        .map_err(database_error)?;
        if rows.len() > 1 {
            return Err(StoreError::InvalidConnectionOperation);
        }
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        let candidate_digest = row
            .try_get::<Option<String>, _>("candidate_digest")
            .map_err(database_error)?
            .ok_or(StoreError::InvalidConnectionOperation)?;
        let orchestration_state = match row
            .try_get::<String, _>("orchestration_state")
            .map_err(database_error)?
            .as_str()
        {
            "runtime_create_pending" => TaskOrchestrationState::RuntimeCreatePending,
            "activation_pending" => TaskOrchestrationState::ActivationPending,
            _ => return Err(StoreError::InvalidConnectionOperation),
        };
        let orchestration_runtime_ownership = match row
            .try_get::<String, _>("orchestration_runtime_ownership")
            .map_err(database_error)?
            .as_str()
        {
            "provisioned" => TaskRuntimeOwnership::Provisioned,
            _ => return Err(StoreError::InvalidConnectionOperation),
        };
        let inert_manifest_digest = row
            .try_get("inert_manifest_digest")
            .map_err(database_error)?;
        let active_manifest_digest = row
            .try_get("active_manifest_digest")
            .map_err(database_error)?;
        let runtime_create_authorized = row
            .try_get("runtime_create_authorized")
            .map_err(database_error)?;
        let activation_effect_authorized = row
            .try_get("activation_effect_authorized")
            .map_err(database_error)?;
        let connection = connection_operation_record(row)?;
        if connection.operation_id != connection.task_uid
            || runtime_name != format!("conn-{}", connection.operation_id.simple())
        {
            return Err(StoreError::InvalidConnectionOperation);
        }
        Ok(Some(ConnectionRuntimeAdmissionRecord {
            connection,
            orchestration_state,
            orchestration_runtime_ownership,
            candidate_digest,
            inert_manifest_digest,
            active_manifest_digest,
            runtime_create_authorized,
            activation_effect_authorized,
        }))
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
                    tasks.phase AS task_phase, COALESCE((SELECT runtime_uid FROM task_runtime_operations runtime_operation WHERE runtime_operation.task_uid = tasks.task_uid), tasks.runtime_uid) AS runtime_uid, \
                    tasks.output_archive, tasks.finalize_requested, tasks.finalized \
             FROM connection_operations operations \
             JOIN task_submissions tasks ON tasks.task_uid = operations.task_uid \
             WHERE COALESCE((SELECT runtime_uid FROM task_runtime_operations runtime_operation WHERE runtime_operation.task_uid = tasks.task_uid), tasks.runtime_uid) = $1 \
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
                    tasks.phase AS task_phase, COALESCE((SELECT runtime_uid FROM task_runtime_operations runtime_operation WHERE runtime_operation.task_uid = tasks.task_uid), tasks.runtime_uid) AS runtime_uid, \
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

    pub async fn task(&self, task_uid: Uuid) -> Result<Option<TaskRecord>, StoreError> {
        let row = sqlx::query(
            "SELECT tasks.*, COALESCE(operations.runtime_uid, tasks.runtime_uid) \
                    AS projected_runtime_uid \
             FROM task_submissions tasks \
             LEFT JOIN task_runtime_operations operations \
               ON operations.task_uid = tasks.task_uid \
             WHERE tasks.task_uid = $1",
        )
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
                 phase = CASE \
                     WHEN orchestration_version = 1 AND phase = 'submitted' THEN 'queued' \
                     WHEN orchestration_version = 2 AND phase = 'submitted' \
                          AND EXISTS (SELECT 1 FROM task_runtime_operations operation \
                              WHERE operation.task_uid = task_submissions.task_uid \
                                AND operation.state = 'active') THEN 'queued' \
                     ELSE phase \
                 END, \
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
            "SELECT tasks.*, COALESCE(orchestration.runtime_uid, tasks.runtime_uid) \
                    AS projected_runtime_uid \
             FROM task_submissions tasks \
             LEFT JOIN task_runtime_operations orchestration \
               ON orchestration.task_uid = tasks.task_uid \
             WHERE tasks.task_uid = $1 AND tasks.submitter_service = $2 \
               AND tasks.owner_user_id = $3 AND tasks.identity_binding_state = 'bound' \
               AND NOT EXISTS (SELECT 1 FROM connection_operations operations \
                   WHERE operations.task_uid = tasks.task_uid)",
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
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let current = task_in_transaction_for_update(&mut transaction, task_uid).await?;
        if current.submitter_service != submitter_service
            || current.owner_user_id.as_deref() != Some(owner_user_id)
            || current.identity_binding_state != "bound"
        {
            return Err(StoreError::TaskNotFound);
        }
        if current.finalized {
            transaction.commit().await.map_err(database_error)?;
            return self.task(task_uid).await?.ok_or(StoreError::TaskNotFound);
        }
        if current.orchestration_version == 2 {
            task_runtime_operation_in_transaction_for_update(&mut transaction, task_uid).await?;
            fence_task_execution_for_cleanup(&mut transaction, task_uid).await?;
        }
        let result = sqlx::query(
            "UPDATE task_submissions \
             SET finalize_requested = true, \
                 cancel_requested = cancel_requested \
                     OR phase NOT IN ('succeeded', 'failed', 'cancelled'), \
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
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        if result.rows_affected() != 1 {
            return Err(StoreError::TaskNotFound);
        }
        transaction.commit().await.map_err(database_error)?;
        self.task(task_uid).await?.ok_or(StoreError::TaskNotFound)
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
            "SELECT tasks.task_uid, COALESCE(orchestration.runtime_uid, tasks.runtime_uid) AS runtime_uid, \
                    tasks.runtime_namespace, tasks.runtime_name \
             FROM task_submissions tasks \
             LEFT JOIN task_runtime_operations orchestration \
               ON orchestration.task_uid = tasks.task_uid \
             WHERE tasks.owner_user_id = $1 AND tasks.submitter_service = $2 \
               AND tasks.identity_binding_state = 'bound' \
               AND COALESCE(orchestration.runtime_uid, tasks.runtime_uid) IS NOT NULL \
               AND tasks.phase = 'running' AND NOT tasks.finalized \
               AND (orchestration.task_uid IS NULL OR orchestration.state = 'active') \
               AND NOT EXISTS (SELECT 1 FROM connection_operations operations \
                   WHERE operations.task_uid = tasks.task_uid) \
             ORDER BY tasks.created_at, tasks.task_uid \
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

    /// Commits a validated provider-control result and finalization request together. Raw bridge
    /// output is cleared in the same transaction so OAuth continuation material cannot remain in
    /// generic task storage after extraction.
    /// Internal output retirement preserves the Task's execution result identity.
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

    pub async fn task_by_idempotency(
        &self,
        submitter_service: &str,
        owner_user_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<TaskRecord>, StoreError> {
        let row = sqlx::query(
            "SELECT tasks.*, COALESCE(orchestration.runtime_uid, tasks.runtime_uid) \
                    AS projected_runtime_uid \
             FROM task_submissions tasks \
             LEFT JOIN task_runtime_operations orchestration \
               ON orchestration.task_uid = tasks.task_uid \
             WHERE tasks.submitter_service = $1 AND tasks.owner_user_id = $2 \
               AND tasks.idempotency_key = $3 AND tasks.identity_binding_state = 'bound'",
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
    pub task_uid: Uuid,
    pub operation_id: Uuid,
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
    /// Server-resolved expected Kubernetes UID for a shared runtime. The orchestrator must
    /// independently observe it before it becomes the bound runtime projection.
    pub runtime_uid: Option<&'a str>,
    pub runtime_namespace: &'a str,
    pub runtime_name: &'a str,
    pub runtime_ownership: steward_types::RuntimeOwnership,
    pub runtime_spec: &'a AgentRuntimeSpec,
    pub agent_command: &'a [String],
    pub execution_binding: Option<&'a TaskExecutionBinding>,
    pub envelope_revision: i64,
    pub service_envelope: &'a Envelope,
    pub service_envelope_digest: &'a str,
    pub candidate_digest: &'a str,
    pub admission_decision: &'a AdmissionDecision,
    pub inert_manifest_digest: &'a str,
    pub active_manifest_digest: &'a str,
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

fn validate_task_runtime_binding(request: &TaskReservationRequest<'_>) -> Result<(), StoreError> {
    match (request.runtime_ownership, request.runtime_uid) {
        (steward_types::RuntimeOwnership::Adopted, Some(runtime_uid))
            if !runtime_uid.is_empty() =>
        {
            Ok(())
        }
        (steward_types::RuntimeOwnership::Provisioned, None) => Ok(()),
        _ => Err(StoreError::InvalidTaskTransition),
    }
}

fn validate_task_orchestration_reservation(
    request: &TaskReservationRequest<'_>,
) -> Result<(), StoreError> {
    if request.service_envelope.revision != request.envelope_revision
        || !valid_sha256_reference(request.candidate_digest)
        || !valid_sha256_reference(request.service_envelope_digest)
        || !valid_sha256_reference(request.inert_manifest_digest)
        || !valid_sha256_reference(request.active_manifest_digest)
        || (request.runtime_ownership == steward_types::RuntimeOwnership::Provisioned
            && !request
                .runtime_name
                .ends_with(&request.operation_id.simple().to_string()))
    {
        return Err(StoreError::InvalidTaskTransition);
    }
    Ok(())
}

fn task_reservation_matches(
    record: &TaskRecord,
    operation: &TaskRuntimeOperationRecord,
    request: &TaskReservationRequest<'_>,
    admission_text: &str,
    deltas: &[AdmissionDelta],
) -> bool {
    let same_server_identity = operation.operation_id != request.operation_id
        || (operation.runtime_name == request.runtime_name
            && operation.inert_manifest_digest == request.inert_manifest_digest
            && operation.active_manifest_digest == request.active_manifest_digest);
    record.submitter_service == request.submitter_service
        && record.acting_user.as_deref() == request.acting_user
        && record.acting_user_id.as_deref() == request.acting_user_id
        && record.owner == request.owner
        && record.owner_user_id.as_deref() == Some(request.owner_user_id)
        && record.workflow == request.workflow
        && record.workflow_name.as_deref() == request.workflow_name
        && record.workflow_version == request.workflow_version
        && record.workflow_digest.as_deref() == request.workflow_digest
        && record.user_envelope_instance_id.as_deref() == request.user_envelope_instance_id
        && record.user_envelope_revision == request.user_envelope_revision
        && record.user_envelope_digest.as_deref() == request.user_envelope_digest
        && record.coding_agent_runtime == request.coding_agent_runtime
        && record.runtime_uid.is_none()
        && record.runtime_namespace == request.runtime_namespace
        && record.runtime_name == operation.runtime_name
        && record.runtime_ownership == request.runtime_ownership
        && operation.runtime_ownership == task_runtime_ownership(request)
        && operation.expected_runtime_uid.as_deref() == request.runtime_uid
        && record.runtime_spec == *request.runtime_spec
        && record.agent_command == request.agent_command
        && record.execution_binding.as_ref() == request.execution_binding
        && record.envelope_revision == request.envelope_revision
        && record.orchestration_version == 2
        && record.orchestration_operation_id == Some(operation.operation_id)
        && record.candidate_digest.as_deref() == Some(request.candidate_digest)
        && record.service_envelope_digest.as_deref() == Some(request.service_envelope_digest)
        && record.original_admission_decision.as_deref() == Some(admission_text)
        && record.original_admission_deltas.as_deref() == Some(deltas)
        && same_server_identity
}

fn task_runtime_ownership(request: &TaskReservationRequest<'_>) -> TaskRuntimeOwnership {
    if request
        .execution_binding
        .is_some_and(|binding| matches!(binding, TaskExecutionBinding::Resident(_)))
    {
        TaskRuntimeOwnership::Resident
    } else {
        match request.runtime_ownership {
            steward_types::RuntimeOwnership::Provisioned => TaskRuntimeOwnership::Provisioned,
            steward_types::RuntimeOwnership::Adopted => TaskRuntimeOwnership::Adopted,
        }
    }
}

fn validate_connection_operation_request(
    request: &ConnectionOperationReservationRequest<'_>,
) -> Result<(), StoreError> {
    validate_task_identity_binding(&request.task)?;
    validate_task_version_pins(&request.task)?;
    validate_task_runtime_binding(&request.task)?;
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

fn inert_task_runtime_spec(spec: &AgentRuntimeSpec, envelope: &Envelope) -> AgentRuntimeSpec {
    let mut inert = spec.clone();
    inert.llms.clear();
    inert.tools.clear();
    inert.budget.monthly_limit = "0".to_owned();
    inert.budget.single_run_limit = Some("0".to_owned());
    inert.budget.currency = envelope.spec.budget.currency.clone();
    inert
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
                        || request.runtime_uid != Some(resident.runtime_uid.0.as_str())
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

#[cfg(test)]
mod task_execution_binding_tests {
    use steward_admission::{AdmissionDecision, Envelope, EnvelopeSpec};
    use steward_types::{
        AgentRuntimeSpec, AgentType, Budget, CanonicalUserId, Duration, Email, Principal,
        ResidentExecutionBinding, RunnerRequirements, RuntimeId, RuntimeOwnership,
        TASK_EXECUTION_BINDING_SCHEMA_VERSION, TaskExecutionBinding,
    };
    use uuid::Uuid;

    use super::{StoreError, TaskReservationRequest, validate_task_version_pins};

    #[test]
    fn resident_reservation_uid_must_match_its_lease_binding() -> Result<(), String> {
        let owner = CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?;
        let binding = TaskExecutionBinding::Resident(ResidentExecutionBinding {
            schema_version: TASK_EXECUTION_BINDING_SCHEMA_VERSION.to_owned(),
            binding_id: "resident-agent-instance-v1".to_owned(),
            binding_digest: format!("sha256:{}", "a".repeat(64)),
            owner_user_id: owner,
            agent_instance_id: "agent-instance-01".to_owned(),
            agent_instance_revision: 1,
            runtime_uid: RuntimeId("runtime-uid-a".to_owned()),
            runtime_spec_digest: format!("sha256:{}", "b".repeat(64)),
            standing_authority_digest: format!("sha256:{}", "c".repeat(64)),
            deployment_binding_digest: format!("sha256:{}", "d".repeat(64)),
            freshness_generation: 1,
        });
        let spec = AgentRuntimeSpec {
            principal: Principal::User {
                acting_user: Email("alice@example.com".to_owned()),
            },
            owner: Email("alice@example.com".to_owned()),
            canonical_authority: None,
            agent_type: AgentType {
                name: "agent@1.0.0".to_owned(),
            },
            llms: Vec::new(),
            tools: Vec::new(),
            budget: Budget {
                monthly_limit: "1.00".to_owned(),
                single_run_limit: None,
                currency: "USD".to_owned(),
            },
            ttl: Duration("1h".to_owned()),
            runner: RunnerRequirements::default(),
            bindings: None,
        };
        let command = Vec::new();
        let envelope = Envelope {
            revision: 1,
            spec: EnvelopeSpec {
                llms: spec.llms.clone(),
                tools: spec.tools.clone(),
                budget: spec.budget.clone(),
                ttl: spec.ttl.clone(),
                runner: spec.runner.clone(),
            },
        };
        let digest = format!("sha256:{}", "e".repeat(64));
        let manifest_digest = format!("sha256:{}", "f".repeat(64));
        let admission = AdmissionDecision::Admit;
        let task_uid = Uuid::new_v4();
        let operation_id = Uuid::new_v4();
        let request = TaskReservationRequest {
            task_uid,
            operation_id,
            idempotency_key: "resident-mismatch",
            submitter_service: "steward-run",
            acting_user: Some("alice@example.com"),
            acting_user_id: Some("usr_0123456789abcdef0123456789abcdef"),
            owner: "alice@example.com",
            owner_user_id: "usr_0123456789abcdef0123456789abcdef",
            workflow: "code-review",
            workflow_name: None,
            workflow_version: None,
            workflow_digest: None,
            user_envelope_instance_id: None,
            user_envelope_revision: None,
            user_envelope_digest: None,
            coding_agent_runtime: "agent@1.0.0",
            runtime_uid: Some("runtime-uid-b"),
            runtime_namespace: "team-a",
            runtime_name: "runtime-a",
            runtime_ownership: RuntimeOwnership::Adopted,
            runtime_spec: &spec,
            agent_command: &command,
            execution_binding: Some(&binding),
            envelope_revision: 1,
            service_envelope: &envelope,
            service_envelope_digest: &digest,
            candidate_digest: &digest,
            admission_decision: &admission,
            inert_manifest_digest: &manifest_digest,
            active_manifest_digest: &manifest_digest,
        };

        assert_eq!(
            validate_task_version_pins(&request),
            Err(StoreError::InvalidTaskIdentityBinding),
            "a resident Task row must not disagree with its immutable lease UID"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TaskReservation {
    pub inserted: bool,
    pub record: TaskRecord,
    pub operation: TaskRuntimeOperationRecord,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskOrchestrationState {
    IntentRecorded,
    RuntimeCreatePending,
    RuntimeObserved,
    ApprovalPending,
    ActivationPending,
    Active,
    CleanupPending,
    Finalized,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskRuntimeOwnership {
    Provisioned,
    Adopted,
    Resident,
}

impl TaskRuntimeOwnership {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Provisioned => "provisioned",
            Self::Adopted => "adopted",
            Self::Resident => "resident",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskRuntimeOperationRecord {
    pub task_uid: Uuid,
    pub operation_id: Uuid,
    pub state: TaskOrchestrationState,
    pub generation: i64,
    pub runtime_ownership: TaskRuntimeOwnership,
    pub runtime_namespace: String,
    pub runtime_name: String,
    pub inert_manifest_digest: String,
    pub active_manifest_digest: String,
    pub expected_runtime_uid: Option<String>,
    pub runtime_uid: Option<String>,
    pub runtime_resource_version: Option<String>,
    pub activation_authority_kind: Option<String>,
    pub activation_envelope_revision: Option<i64>,
    pub activation_envelope_digest: Option<String>,
    pub approval_id: Option<Uuid>,
    pub retry_at: Option<String>,
    pub last_error_code: Option<String>,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<String>,
    pub requested_at: String,
    pub runtime_create_authorized_at: Option<String>,
    pub observed_at: Option<String>,
    pub activation_effect_authorized_at: Option<String>,
    pub activated_at: Option<String>,
    pub cleanup_requested_at: Option<String>,
    pub runtime_absent_observed_at: Option<String>,
    pub projections_absent_observed_at: Option<String>,
    pub finalized_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TaskOrchestrationWorkItem {
    pub task: TaskRecord,
    pub operation: TaskRuntimeOperationRecord,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ApprovalDeliveryWorkItem {
    pub effect_id: Uuid,
    pub task_uid: Uuid,
    pub operation_id: Uuid,
    pub approval_id: Uuid,
    pub generation: i64,
    pub delivery_invoked: bool,
    pub idempotency_key: String,
    pub runtime_uid: String,
    pub actor: String,
    pub member_role: String,
    pub deltas: Vec<AdmissionDelta>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalDeliveryTransition {
    Applied,
    AlreadyApplied,
    Superseded,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskOperationTransition {
    Applied(TaskRuntimeOperationRecord),
    AlreadyApplied(TaskRuntimeOperationRecord),
    Superseded(TaskRuntimeOperationRecord),
    AuthorityInactive {
        current: TaskRuntimeOperationRecord,
        reason: &'static str,
    },
    InvariantViolation {
        current: TaskRuntimeOperationRecord,
        reason: &'static str,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskCleanupCause<'a> {
    FinalizationRequested,
    Cancelled,
    AuthorityInactive(&'a str),
    Failed(&'a str),
    ExecutionOutcomeUnknown,
}

impl<'a> TaskCleanupCause<'a> {
    fn task_outcome(self) -> Result<(&'static str, Option<&'a str>), StoreError> {
        match self {
            Self::FinalizationRequested | Self::Cancelled => Ok(("cancelled", None)),
            Self::AuthorityInactive(reason) | Self::Failed(reason) if !reason.is_empty() => {
                Ok(("failed", Some(reason)))
            }
            Self::ExecutionOutcomeUnknown => Ok(("failed", Some("execution_outcome_unknown"))),
            Self::AuthorityInactive(_) | Self::Failed(_) => Err(StoreError::InvalidTaskTransition),
        }
    }

    const fn event_kind(self) -> &'static str {
        match self {
            Self::FinalizationRequested => "finalization_cleanup_requested",
            Self::Cancelled => "cancellation_cleanup_requested",
            Self::AuthorityInactive(_) => "authority_inactive_cleanup_requested",
            Self::Failed(_) => "failure_cleanup_requested",
            Self::ExecutionOutcomeUnknown => "execution_outcome_unknown_cleanup_requested",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskActivationObservation<'a> {
    pub runtime_uid: &'a str,
    pub resource_version: &'a str,
    pub active_manifest_digest: &'a str,
    pub provider_set_ready: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskCleanupObservation {
    pub exact_runtime_absent: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskExecutionAttemptState {
    StartPending,
    NotStarted,
    Running,
    Succeeded,
    Failed,
    CancelPending,
    OutcomeUnknown,
}

impl TaskExecutionAttemptState {
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::NotStarted | Self::Succeeded | Self::Failed | Self::OutcomeUnknown
        )
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StartPending => "start_pending",
            Self::NotStarted => "not_started",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::CancelPending => "cancel_pending",
            Self::OutcomeUnknown => "outcome_unknown",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskExecutionAttemptRecord {
    pub attempt_id: Uuid,
    pub task_uid: Uuid,
    pub operation_id: Uuid,
    pub runtime_uid: String,
    pub active_manifest_digest: String,
    pub command_digest: String,
    pub input_digest: String,
    pub state: TaskExecutionAttemptState,
    pub generation: i64,
    pub adapter_observation_id: Option<String>,
    pub result_digest: Option<String>,
    pub result_reference: Option<String>,
    pub retry_at: Option<String>,
    pub last_error_code: Option<String>,
    pub start_invoked_at: Option<String>,
    pub start_observation_deadline_at: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskExecutionTransition {
    Created(TaskExecutionAttemptRecord),
    Applied(TaskExecutionAttemptRecord),
    AlreadyApplied(TaskExecutionAttemptRecord),
    Superseded(TaskExecutionAttemptRecord),
    AuthorityInactive {
        attempt: Option<TaskExecutionAttemptRecord>,
        reason: &'static str,
    },
    InvariantViolation {
        attempt: Option<TaskExecutionAttemptRecord>,
        reason: &'static str,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskExecutionObservation<'a> {
    Accepted {
        adapter_observation_id: &'a str,
    },
    Running {
        adapter_observation_id: &'a str,
    },
    Succeeded {
        adapter_observation_id: &'a str,
        result_digest: &'a str,
        result_reference: &'a str,
        output_archive: &'a [u8],
    },
    Failed {
        adapter_observation_id: &'a str,
        reason: &'a str,
    },
    OutcomeUnknown {
        reason: &'a str,
    },
}

impl TaskExecutionObservation<'_> {
    fn is_valid(self) -> bool {
        match self {
            Self::Accepted {
                adapter_observation_id,
            }
            | Self::Running {
                adapter_observation_id,
            } => !adapter_observation_id.is_empty(),
            Self::Succeeded {
                adapter_observation_id,
                result_digest,
                result_reference,
                ..
            } => {
                !adapter_observation_id.is_empty()
                    && valid_sha256_reference(result_digest)
                    && !result_reference.is_empty()
            }
            Self::Failed {
                adapter_observation_id,
                reason,
            } => !adapter_observation_id.is_empty() && !reason.is_empty(),
            Self::OutcomeUnknown { reason } => !reason.is_empty(),
        }
    }
}

impl TaskOrchestrationState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IntentRecorded => "intent_recorded",
            Self::RuntimeCreatePending => "runtime_create_pending",
            Self::RuntimeObserved => "runtime_observed",
            Self::ApprovalPending => "approval_pending",
            Self::ActivationPending => "activation_pending",
            Self::Active => "active",
            Self::CleanupPending => "cleanup_pending",
            Self::Finalized => "finalized",
        }
    }

    pub const fn allows_transition_to(self, next: Self) -> bool {
        if matches!(self, Self::Finalized) {
            return false;
        }
        if self as u8 == next as u8 || matches!(next, Self::CleanupPending) {
            return true;
        }
        matches!(
            (self, next),
            (
                Self::IntentRecorded,
                Self::RuntimeCreatePending | Self::RuntimeObserved | Self::Finalized
            ) | (Self::RuntimeCreatePending, Self::RuntimeObserved)
                | (
                    Self::RuntimeObserved,
                    Self::ApprovalPending | Self::ActivationPending
                )
                | (Self::ApprovalPending, Self::ActivationPending)
                | (Self::ActivationPending, Self::Active)
                | (Self::CleanupPending, Self::Finalized)
        )
    }
}

#[cfg(test)]
mod task_orchestration_state_tests {
    use super::TaskOrchestrationState::{
        ActivationPending, Active, ApprovalPending, CleanupPending, Finalized, IntentRecorded,
        RuntimeCreatePending, RuntimeObserved,
    };

    #[test]
    fn orchestration_state_only_moves_forward_or_into_cleanup() {
        for (from, to) in [
            (IntentRecorded, RuntimeCreatePending),
            (IntentRecorded, RuntimeObserved),
            (IntentRecorded, Finalized),
            (RuntimeCreatePending, RuntimeObserved),
            (RuntimeObserved, ApprovalPending),
            (RuntimeObserved, ActivationPending),
            (ApprovalPending, ActivationPending),
            (ActivationPending, Active),
            (Active, CleanupPending),
            (CleanupPending, Finalized),
        ] {
            assert!(
                from.allows_transition_to(to),
                "the durable orchestration transition {from:?} -> {to:?} must be allowed"
            );
        }

        for from in [
            IntentRecorded,
            RuntimeCreatePending,
            RuntimeObserved,
            ApprovalPending,
            ActivationPending,
        ] {
            assert!(
                from.allows_transition_to(CleanupPending),
                "cancellation and authority loss must move {from:?} into cleanup"
            );
        }

        for (from, to) in [
            (RuntimeObserved, RuntimeCreatePending),
            (ApprovalPending, RuntimeObserved),
            (Active, ActivationPending),
            (CleanupPending, Active),
            (Finalized, CleanupPending),
        ] {
            assert!(
                !from.allows_transition_to(to),
                "the durable orchestration transition {from:?} -> {to:?} must be rejected"
            );
        }
    }
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
    pub envelope_revision: i64,
    pub orchestration_version: i16,
    pub orchestration_operation_id: Option<Uuid>,
    pub candidate_digest: Option<String>,
    pub service_envelope_digest: Option<String>,
    pub original_admission_decision: Option<String>,
    pub original_admission_deltas: Option<Vec<AdmissionDelta>>,
    pub input_archive: Option<Vec<u8>>,
    pub output_archive: Option<Vec<u8>>,
    pub execute_requested: bool,
    pub cancel_requested: bool,
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

#[derive(Clone, Debug, PartialEq)]
pub struct ConnectionRuntimeAdmissionRecord {
    pub connection: ConnectionOperationRecord,
    pub orchestration_state: TaskOrchestrationState,
    pub orchestration_runtime_ownership: TaskRuntimeOwnership,
    pub candidate_digest: String,
    pub inert_manifest_digest: String,
    pub active_manifest_digest: String,
    pub runtime_create_authorized: bool,
    pub activation_effect_authorized: bool,
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
    pub task_uid: Option<Uuid>,
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

pub struct TaskAdmissionLookup {
    pub task_uid: Uuid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionApprovalState {
    Pending,
    Approved,
    Rejected,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TaskAdmissionRecord {
    pub decision_id: Uuid,
    pub approval_id: Uuid,
    pub runtime_uid: String,
    pub state: AdmissionApprovalState,
    pub decision_key: Option<String>,
    pub evidence_url: Option<String>,
    pub deltas: Vec<AdmissionDelta>,
    pub proposed_spec: AgentRuntimeSpec,
}

enum EffectiveTaskAuthority {
    Baseline { envelope_revision: i64 },
    Pending,
    Active(Box<GrantApplication>),
    Inactive,
}

fn operation_authority_is_current(
    authority: &EffectiveTaskAuthority,
    operation: &TaskRuntimeOperationRecord,
) -> bool {
    match (authority, operation.activation_authority_kind.as_deref()) {
        (EffectiveTaskAuthority::Baseline { .. }, Some("baseline")) => true,
        (EffectiveTaskAuthority::Baseline { envelope_revision }, Some("internal")) => {
            operation.activation_envelope_revision == Some(*envelope_revision)
        }
        (EffectiveTaskAuthority::Active(application), Some("grant")) => {
            operation.approval_id == Some(application.approval_id)
        }
        _ => false,
    }
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
    pub orchestration_operation_id: Option<Uuid>,
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

fn task_admission_record(row: sqlx::postgres::PgRow) -> Result<TaskAdmissionRecord, StoreError> {
    let state = match row
        .try_get::<String, _>("state")
        .map_err(database_error)?
        .as_str()
    {
        "pending" => AdmissionApprovalState::Pending,
        "approved" => AdmissionApprovalState::Approved,
        "rejected" => AdmissionApprovalState::Rejected,
        value => {
            return Err(StoreError::Database(format!(
                "persisted approval has unsupported state {value}"
            )));
        }
    };
    let Json(deltas) = row
        .try_get::<Json<Vec<AdmissionDelta>>, _>("deltas")
        .map_err(database_error)?;
    let Json(proposed_spec) = row
        .try_get::<Json<AgentRuntimeSpec>, _>("proposed_spec")
        .map_err(database_error)?;
    Ok(TaskAdmissionRecord {
        decision_id: row.try_get("decision_id").map_err(database_error)?,
        approval_id: row.try_get("approval_id").map_err(database_error)?,
        runtime_uid: row.try_get("runtime_uid").map_err(database_error)?,
        state,
        decision_key: row.try_get("decision_key").map_err(database_error)?,
        evidence_url: row.try_get("evidence_url").map_err(database_error)?,
        deltas,
        proposed_spec,
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
        runtime_uid: row
            .try_get("projected_runtime_uid")
            .or_else(|_| row.try_get("runtime_uid"))
            .map_err(database_error)?,
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
        envelope_revision: row.try_get("envelope_revision").map_err(database_error)?,
        orchestration_version: row
            .try_get("orchestration_version")
            .map_err(database_error)?,
        orchestration_operation_id: row
            .try_get("orchestration_operation_id")
            .map_err(database_error)?,
        candidate_digest: row.try_get("candidate_digest").map_err(database_error)?,
        service_envelope_digest: row
            .try_get("service_envelope_digest")
            .map_err(database_error)?,
        original_admission_decision: row
            .try_get("original_admission_decision")
            .map_err(database_error)?,
        original_admission_deltas: row
            .try_get::<Option<Json<Vec<AdmissionDelta>>>, _>("original_admission_deltas")
            .map_err(database_error)?
            .map(|deltas| deltas.0),
        input_archive: row.try_get("input_archive").map_err(database_error)?,
        output_archive: row.try_get("output_archive").map_err(database_error)?,
        execute_requested: row.try_get("execute_requested").map_err(database_error)?,
        cancel_requested: row.try_get("cancel_requested").map_err(database_error)?,
        finalize_requested: row.try_get("finalize_requested").map_err(database_error)?,
        finalized: row.try_get("finalized").map_err(database_error)?,
        failure_reason: row.try_get("failure_reason").map_err(database_error)?,
    })
}

const TASK_EXECUTION_ATTEMPT_SELECT_BY_ID: &str = "SELECT attempt_id, task_uid, operation_id, runtime_uid, active_manifest_digest, \
            command_digest, input_digest, state, generation, adapter_observation_id, \
            result_digest, result_reference, retry_at::text AS retry_at, last_error_code, \
            start_invoked_at::text AS start_invoked_at, \
            start_observation_deadline_at::text AS start_observation_deadline_at, \
            started_at::text AS started_at, finished_at::text AS finished_at \
     FROM task_execution_attempts WHERE attempt_id = $1";

const TASK_EXECUTION_ATTEMPT_SELECT_BY_TASK: &str = "SELECT attempt_id, task_uid, operation_id, runtime_uid, active_manifest_digest, \
            command_digest, input_digest, state, generation, adapter_observation_id, \
            result_digest, result_reference, retry_at::text AS retry_at, last_error_code, \
            start_invoked_at::text AS start_invoked_at, \
            start_observation_deadline_at::text AS start_observation_deadline_at, \
            started_at::text AS started_at, finished_at::text AS finished_at \
     FROM task_execution_attempts WHERE task_uid = $1";

const TASK_EXECUTION_ATTEMPT_SELECT_ACTIVE_BY_RUNTIME: &str = "SELECT attempt_id, task_uid, operation_id, runtime_uid, active_manifest_digest, \
            command_digest, input_digest, state, generation, adapter_observation_id, \
            result_digest, result_reference, retry_at::text AS retry_at, last_error_code, \
            start_invoked_at::text AS start_invoked_at, \
            start_observation_deadline_at::text AS start_observation_deadline_at, \
            started_at::text AS started_at, finished_at::text AS finished_at \
     FROM task_execution_attempts WHERE attempt_id = ( \
         SELECT attempt_id FROM task_runtime_execution_leases WHERE runtime_uid = $1)";

const TASK_RUNTIME_OPERATION_SELECT_BY_TASK: &str = "SELECT task_uid, operation_id, state, generation, runtime_ownership, \
            runtime_namespace, runtime_name, inert_manifest_digest, active_manifest_digest, \
            expected_runtime_uid, \
            runtime_uid, runtime_resource_version, activation_authority_kind, \
            activation_envelope_revision, activation_envelope_digest, approval_id, \
            retry_at::text AS retry_at, last_error_code, lease_owner, \
            lease_expires_at::text AS lease_expires_at, requested_at::text AS requested_at, \
            runtime_create_authorized_at::text AS runtime_create_authorized_at, \
            observed_at::text AS observed_at, activated_at::text AS activated_at, \
            activation_effect_authorized_at::text AS activation_effect_authorized_at, \
            cleanup_requested_at::text AS cleanup_requested_at, \
            runtime_absent_observed_at::text AS runtime_absent_observed_at, \
            projections_absent_observed_at::text AS projections_absent_observed_at, \
            finalized_at::text AS finalized_at \
     FROM task_runtime_operations WHERE task_uid = $1";

const TASK_RUNTIME_OPERATION_SELECT_DUE: &str = "SELECT task_uid, operation_id, state, generation, runtime_ownership, \
            runtime_namespace, runtime_name, inert_manifest_digest, active_manifest_digest, \
            expected_runtime_uid, \
            runtime_uid, runtime_resource_version, activation_authority_kind, \
            activation_envelope_revision, activation_envelope_digest, approval_id, \
            retry_at::text AS retry_at, last_error_code, lease_owner, \
            lease_expires_at::text AS lease_expires_at, requested_at::text AS requested_at, \
            runtime_create_authorized_at::text AS runtime_create_authorized_at, \
            observed_at::text AS observed_at, activated_at::text AS activated_at, \
            activation_effect_authorized_at::text AS activation_effect_authorized_at, \
            cleanup_requested_at::text AS cleanup_requested_at, \
            runtime_absent_observed_at::text AS runtime_absent_observed_at, \
            projections_absent_observed_at::text AS projections_absent_observed_at, \
            finalized_at::text AS finalized_at \
     FROM task_runtime_operations \
     WHERE (state <> 'finalized' AND (retry_at IS NULL OR retry_at <= now())) \
        OR (runtime_ownership IN ('adopted', 'resident') AND EXISTS ( \
            SELECT 1 FROM task_execution_attempts attempts \
            JOIN task_runtime_execution_leases leases ON leases.attempt_id = attempts.attempt_id \
            WHERE attempts.task_uid = task_runtime_operations.task_uid AND attempts.state = 'outcome_unknown')) \
     ORDER BY COALESCE(retry_at, requested_at), task_uid";

/// The caller holds the Task row lock (and operation lock when changing it).
/// Start authorization takes the same lock, so either it wins and cancellation
/// retains ownership, or this transition wins and no stale worker may start.
async fn fence_task_execution_for_cleanup(
    transaction: &mut sqlx::Transaction<'_, Postgres>,
    task_uid: Uuid,
) -> Result<(), StoreError> {
    sqlx::query(
        "UPDATE task_execution_attempts \
         SET state = CASE WHEN start_invoked_at IS NULL THEN 'not_started' ELSE 'cancel_pending' END, \
             finished_at = CASE WHEN start_invoked_at IS NULL THEN now() ELSE finished_at END, \
             last_error_code = CASE WHEN start_invoked_at IS NULL THEN 'start_fenced_by_cleanup' ELSE last_error_code END, \
             generation = generation + 1, updated_at = now() \
         WHERE task_uid = $1 AND state IN ('start_pending', 'running')",
    )
    .bind(task_uid)
    .execute(&mut **transaction)
    .await
    .map_err(database_error)?;
    sqlx::query(
        "INSERT INTO task_execution_retirements (attempt_id, runtime_uid, evidence_kind) \
         SELECT attempt_id, runtime_uid, 'never_authorized' FROM task_execution_attempts \
         WHERE task_uid = $1 AND state = 'not_started' ON CONFLICT (attempt_id) DO NOTHING",
    )
    .bind(task_uid)
    .execute(&mut **transaction)
    .await
    .map_err(database_error)?;
    sqlx::query(
        "DELETE FROM task_runtime_execution_leases leases USING task_execution_retirements retired \
         JOIN task_execution_attempts attempts ON attempts.attempt_id = retired.attempt_id \
         WHERE attempts.task_uid = $1 AND leases.attempt_id = retired.attempt_id \
             AND leases.runtime_uid = retired.runtime_uid",
    )
    .bind(task_uid)
    .execute(&mut **transaction)
    .await
    .map_err(database_error)?;
    Ok(())
}

/// Late terminal evidence retires ownership without revising the Task or attempt
/// outcome. Absence, deadlines and outcome_unknown never enter this path.
async fn record_terminal_execution_retirement(
    transaction: &mut sqlx::Transaction<'_, Postgres>,
    attempt: &TaskExecutionAttemptRecord,
    observation: TaskExecutionObservation<'_>,
) -> Result<(), StoreError> {
    let adapter_observation_id = match observation {
        TaskExecutionObservation::Succeeded {
            adapter_observation_id,
            ..
        }
        | TaskExecutionObservation::Failed {
            adapter_observation_id,
            ..
        } => adapter_observation_id,
        _ => return Ok(()),
    };
    sqlx::query(
        "INSERT INTO task_execution_retirements \
         (attempt_id, runtime_uid, evidence_kind, adapter_observation_id) \
         VALUES ($1, $2, 'adapter_terminal', $3) ON CONFLICT (attempt_id) DO NOTHING",
    )
    .bind(attempt.attempt_id)
    .bind(&attempt.runtime_uid)
    .bind(adapter_observation_id)
    .execute(&mut **transaction)
    .await
    .map_err(database_error)?;
    sqlx::query(
        "DELETE FROM task_runtime_execution_leases WHERE attempt_id = $1 AND runtime_uid = $2",
    )
    .bind(attempt.attempt_id)
    .bind(&attempt.runtime_uid)
    .execute(&mut **transaction)
    .await
    .map_err(database_error)?;
    Ok(())
}

async fn task_execution_attempt_in_transaction(
    transaction: &mut sqlx::Transaction<'_, Postgres>,
    attempt_id: Uuid,
    for_update: bool,
) -> Result<Option<TaskExecutionAttemptRecord>, StoreError> {
    let query = if for_update {
        format!("{TASK_EXECUTION_ATTEMPT_SELECT_BY_ID} FOR UPDATE")
    } else {
        TASK_EXECUTION_ATTEMPT_SELECT_BY_ID.to_owned()
    };
    sqlx::query(&query)
        .bind(attempt_id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(database_error)?
        .map(task_execution_attempt_record)
        .transpose()
}

async fn task_execution_attempt_for_task_in_transaction(
    transaction: &mut sqlx::Transaction<'_, Postgres>,
    task_uid: Uuid,
    for_update: bool,
) -> Result<Option<TaskExecutionAttemptRecord>, StoreError> {
    let query = if for_update {
        format!("{TASK_EXECUTION_ATTEMPT_SELECT_BY_TASK} FOR UPDATE")
    } else {
        TASK_EXECUTION_ATTEMPT_SELECT_BY_TASK.to_owned()
    };
    sqlx::query(&query)
        .bind(task_uid)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(database_error)?
        .map(task_execution_attempt_record)
        .transpose()
}

fn task_execution_attempt_record(
    row: sqlx::postgres::PgRow,
) -> Result<TaskExecutionAttemptRecord, StoreError> {
    let state = match row
        .try_get::<String, _>("state")
        .map_err(database_error)?
        .as_str()
    {
        "start_pending" => TaskExecutionAttemptState::StartPending,
        "not_started" => TaskExecutionAttemptState::NotStarted,
        "running" => TaskExecutionAttemptState::Running,
        "succeeded" => TaskExecutionAttemptState::Succeeded,
        "failed" => TaskExecutionAttemptState::Failed,
        "cancel_pending" => TaskExecutionAttemptState::CancelPending,
        "outcome_unknown" => TaskExecutionAttemptState::OutcomeUnknown,
        _ => return Err(StoreError::InvalidTaskTransition),
    };
    Ok(TaskExecutionAttemptRecord {
        attempt_id: row.try_get("attempt_id").map_err(database_error)?,
        task_uid: row.try_get("task_uid").map_err(database_error)?,
        operation_id: row.try_get("operation_id").map_err(database_error)?,
        runtime_uid: row.try_get("runtime_uid").map_err(database_error)?,
        active_manifest_digest: row
            .try_get("active_manifest_digest")
            .map_err(database_error)?,
        command_digest: row.try_get("command_digest").map_err(database_error)?,
        input_digest: row.try_get("input_digest").map_err(database_error)?,
        state,
        generation: row.try_get("generation").map_err(database_error)?,
        adapter_observation_id: row
            .try_get("adapter_observation_id")
            .map_err(database_error)?,
        result_digest: row.try_get("result_digest").map_err(database_error)?,
        result_reference: row.try_get("result_reference").map_err(database_error)?,
        retry_at: row.try_get("retry_at").map_err(database_error)?,
        last_error_code: row.try_get("last_error_code").map_err(database_error)?,
        start_invoked_at: row.try_get("start_invoked_at").map_err(database_error)?,
        start_observation_deadline_at: row
            .try_get("start_observation_deadline_at")
            .map_err(database_error)?,
        started_at: row.try_get("started_at").map_err(database_error)?,
        finished_at: row.try_get("finished_at").map_err(database_error)?,
    })
}

async fn task_runtime_operation_in_transaction(
    transaction: &mut sqlx::Transaction<'_, Postgres>,
    task_uid: Uuid,
) -> Result<TaskRuntimeOperationRecord, StoreError> {
    sqlx::query(TASK_RUNTIME_OPERATION_SELECT_BY_TASK)
        .bind(task_uid)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(database_error)?
        .map(task_runtime_operation_record)
        .transpose()?
        .ok_or(StoreError::TaskNotFound)
}

async fn task_in_transaction_for_update(
    transaction: &mut sqlx::Transaction<'_, Postgres>,
    task_uid: Uuid,
) -> Result<TaskRecord, StoreError> {
    sqlx::query("SELECT * FROM task_submissions WHERE task_uid = $1 FOR UPDATE")
        .bind(task_uid)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(database_error)?
        .map(task_record)
        .transpose()?
        .ok_or(StoreError::TaskNotFound)
}

async fn task_runtime_operation_in_transaction_for_update(
    transaction: &mut sqlx::Transaction<'_, Postgres>,
    task_uid: Uuid,
) -> Result<TaskRuntimeOperationRecord, StoreError> {
    let query = format!("{TASK_RUNTIME_OPERATION_SELECT_BY_TASK} FOR UPDATE");
    sqlx::query(&query)
        .bind(task_uid)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(database_error)?
        .map(task_runtime_operation_record)
        .transpose()?
        .ok_or(StoreError::TaskNotFound)
}

async fn append_task_orchestration_journal(
    transaction: &mut sqlx::Transaction<'_, Postgres>,
    task_uid: Uuid,
    generation: i64,
    state: TaskOrchestrationState,
    event_kind: &str,
    actor: &str,
) -> Result<(), StoreError> {
    let inserted = sqlx::query(
        "INSERT INTO task_orchestration_journal \
         (task_uid, operation_id, generation, state, event_kind, payload, actor) \
         SELECT task_uid, operation_id, $2, $3, $4, '{}'::jsonb, $5 \
         FROM task_runtime_operations WHERE task_uid = $1 AND generation = $2",
    )
    .bind(task_uid)
    .bind(generation)
    .bind(state.as_str())
    .bind(event_kind)
    .bind(actor)
    .execute(&mut **transaction)
    .await
    .map_err(database_error)?
    .rows_affected();
    if inserted == 1 {
        Ok(())
    } else {
        Err(StoreError::InvalidTaskTransition)
    }
}

async fn retire_task_authority_for_cleanup(
    transaction: &mut sqlx::Transaction<'_, Postgres>,
    task_uid: Uuid,
    operation_id: Uuid,
    actor: &str,
) -> Result<bool, StoreError> {
    let delivery_states = sqlx::query(
        "SELECT state, delivery_invoked_at IS NOT NULL AS delivery_invoked FROM external_effect_outbox \
         WHERE task_uid = $1 AND operation_id = $2 FOR UPDATE",
    )
    .bind(task_uid)
    .bind(operation_id)
    .fetch_all(&mut **transaction)
    .await
    .map_err(database_error)?;
    if delivery_states.iter().any(|row| {
        row.try_get::<String, _>("state").is_ok_and(|state| {
            state == "claimed"
                || (state != "delivered"
                    && row.try_get::<bool, _>("delivery_invoked").unwrap_or(true))
        })
    }) {
        return Ok(false);
    }
    sqlx::query(
        "UPDATE external_effect_outbox \
         SET state = 'failed', generation = generation + 1, retry_at = NULL, \
             last_error_code = 'task_cleanup', claimed_by = NULL, claimed_until = NULL, \
             updated_at = now() \
         WHERE task_uid = $1 AND operation_id = $2 AND state = 'pending'",
    )
    .bind(task_uid)
    .bind(operation_id)
    .execute(&mut **transaction)
    .await
    .map_err(database_error)?;
    sqlx::query(
        "UPDATE approvals approvals \
         SET state = 'rejected', decided_by = $3, decided_at = now(), \
             rationale = 'Task cleanup retired pending approval authority' \
         FROM admission_decisions decisions \
         WHERE approvals.admission_decision_id = decisions.id \
           AND decisions.task_uid = $1 AND decisions.orchestration_operation_id = $2 \
           AND approvals.state = 'pending'",
    )
    .bind(task_uid)
    .bind(operation_id)
    .bind(actor)
    .execute(&mut **transaction)
    .await
    .map_err(database_error)?;
    sqlx::query(
        "INSERT INTO grant_revocations (grant_id, revoked_by, reason) \
         SELECT grants.id, $3, 'Task cleanup retired runtime authority' \
         FROM grants \
         JOIN approvals ON approvals.id = grants.approval_id \
         JOIN admission_decisions decisions \
           ON decisions.id = approvals.admission_decision_id \
         LEFT JOIN grant_revocations ON grant_revocations.grant_id = grants.id \
         WHERE decisions.task_uid = $1 AND decisions.orchestration_operation_id = $2 \
           AND grant_revocations.grant_id IS NULL \
         ON CONFLICT (grant_id) DO NOTHING",
    )
    .bind(task_uid)
    .bind(operation_id)
    .bind(actor)
    .execute(&mut **transaction)
    .await
    .map_err(database_error)?;
    let authority_remains = sqlx::query_scalar::<_, bool>(
        "SELECT \
           EXISTS( \
             SELECT 1 FROM approvals \
             JOIN admission_decisions decisions \
               ON decisions.id = approvals.admission_decision_id \
             WHERE decisions.task_uid = $1 \
               AND decisions.orchestration_operation_id = $2 \
               AND approvals.state = 'pending' \
           ) OR EXISTS( \
             SELECT 1 FROM external_effect_outbox \
             WHERE task_uid = $1 AND operation_id = $2 \
               AND state IN ('pending', 'claimed') \
           ) OR EXISTS( \
             SELECT 1 FROM grants \
             JOIN approvals ON approvals.id = grants.approval_id \
             JOIN admission_decisions decisions \
               ON decisions.id = approvals.admission_decision_id \
             LEFT JOIN grant_revocations ON grant_revocations.grant_id = grants.id \
             WHERE decisions.task_uid = $1 \
               AND decisions.orchestration_operation_id = $2 \
               AND grants.expires_at > now() \
               AND grant_revocations.grant_id IS NULL \
           )",
    )
    .bind(task_uid)
    .bind(operation_id)
    .fetch_one(&mut **transaction)
    .await
    .map_err(database_error)?;
    Ok(!authority_remains)
}

fn task_runtime_operation_record(
    row: sqlx::postgres::PgRow,
) -> Result<TaskRuntimeOperationRecord, StoreError> {
    let state = match row
        .try_get::<String, _>("state")
        .map_err(database_error)?
        .as_str()
    {
        "intent_recorded" => TaskOrchestrationState::IntentRecorded,
        "runtime_create_pending" => TaskOrchestrationState::RuntimeCreatePending,
        "runtime_observed" => TaskOrchestrationState::RuntimeObserved,
        "approval_pending" => TaskOrchestrationState::ApprovalPending,
        "activation_pending" => TaskOrchestrationState::ActivationPending,
        "active" => TaskOrchestrationState::Active,
        "cleanup_pending" => TaskOrchestrationState::CleanupPending,
        "finalized" => TaskOrchestrationState::Finalized,
        _ => return Err(StoreError::InvalidTaskTransition),
    };
    let runtime_ownership = match row
        .try_get::<String, _>("runtime_ownership")
        .map_err(database_error)?
        .as_str()
    {
        "provisioned" => TaskRuntimeOwnership::Provisioned,
        "adopted" => TaskRuntimeOwnership::Adopted,
        "resident" => TaskRuntimeOwnership::Resident,
        _ => return Err(StoreError::InvalidTaskTransition),
    };
    Ok(TaskRuntimeOperationRecord {
        task_uid: row.try_get("task_uid").map_err(database_error)?,
        operation_id: row.try_get("operation_id").map_err(database_error)?,
        state,
        generation: row.try_get("generation").map_err(database_error)?,
        runtime_ownership,
        runtime_namespace: row.try_get("runtime_namespace").map_err(database_error)?,
        runtime_name: row.try_get("runtime_name").map_err(database_error)?,
        inert_manifest_digest: row
            .try_get("inert_manifest_digest")
            .map_err(database_error)?,
        active_manifest_digest: row
            .try_get("active_manifest_digest")
            .map_err(database_error)?,
        expected_runtime_uid: row
            .try_get("expected_runtime_uid")
            .map_err(database_error)?,
        runtime_uid: row.try_get("runtime_uid").map_err(database_error)?,
        runtime_resource_version: row
            .try_get("runtime_resource_version")
            .map_err(database_error)?,
        activation_authority_kind: row
            .try_get("activation_authority_kind")
            .map_err(database_error)?,
        activation_envelope_revision: row
            .try_get("activation_envelope_revision")
            .map_err(database_error)?,
        activation_envelope_digest: row
            .try_get("activation_envelope_digest")
            .map_err(database_error)?,
        approval_id: row.try_get("approval_id").map_err(database_error)?,
        retry_at: row.try_get("retry_at").map_err(database_error)?,
        last_error_code: row.try_get("last_error_code").map_err(database_error)?,
        lease_owner: row.try_get("lease_owner").map_err(database_error)?,
        lease_expires_at: row.try_get("lease_expires_at").map_err(database_error)?,
        requested_at: row.try_get("requested_at").map_err(database_error)?,
        runtime_create_authorized_at: row
            .try_get("runtime_create_authorized_at")
            .map_err(database_error)?,
        observed_at: row.try_get("observed_at").map_err(database_error)?,
        activation_effect_authorized_at: row
            .try_get("activation_effect_authorized_at")
            .map_err(database_error)?,
        activated_at: row.try_get("activated_at").map_err(database_error)?,
        cleanup_requested_at: row
            .try_get("cleanup_requested_at")
            .map_err(database_error)?,
        runtime_absent_observed_at: row
            .try_get("runtime_absent_observed_at")
            .map_err(database_error)?,
        projections_absent_observed_at: row
            .try_get("projections_absent_observed_at")
            .map_err(database_error)?,
        finalized_at: row.try_get("finalized_at").map_err(database_error)?,
    })
}

const AGENT_RUN_SELECT: &str = "SELECT tasks.task_uid, tasks.submitter_service, tasks.acting_user, tasks.owner, tasks.owner_user_id, \
            tasks.workflow, tasks.workflow_name, tasks.workflow_version, tasks.workflow_digest, \
            tasks.user_envelope_instance_id, tasks.user_envelope_revision, tasks.user_envelope_digest, \
            tasks.coding_agent_runtime, COALESCE(orchestration.runtime_uid, tasks.runtime_uid) AS runtime_uid, \
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
     LEFT JOIN task_runtime_operations orchestration \
       ON orchestration.task_uid = tasks.task_uid \
     LEFT JOIN LATERAL ( \
         SELECT observation.observed_amount::text AS observed_amount, \
                observation.currency, observation.exhausted, \
                to_char(observation.at AT TIME ZONE 'UTC', \
                        'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS observed_at \
         FROM spend_observations observation \
         WHERE observation.runtime_uid = COALESCE(orchestration.runtime_uid, tasks.runtime_uid) \
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
