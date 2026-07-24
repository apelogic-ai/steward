//! Append-only operational history and approval-queue persistence.

use std::error::Error;
use std::fmt;

use sqlx::types::Json;
use sqlx::{PgPool, Row};
use steward_admission::{AdmissionDelta, Envelope, EnvelopeSpec};
use steward_types::AgentRuntimeSpec;
use uuid::Uuid;

#[derive(Clone)]
pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
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

    pub async fn insert_envelope(
        &self,
        member_role: &str,
        envelope: &Envelope,
        authored_by: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO envelopes \
             (scope_kind, scope_ref, revision, spec, authored_by) \
             VALUES ('member_role', $1, $2, $3, $4)",
        )
        .bind(member_role)
        .bind(envelope.revision)
        .bind(Json(&envelope.spec))
        .bind(authored_by)
        .execute(&self.pool)
        .await
        .map_err(database_error)?;
        Ok(())
    }

    pub async fn latest_envelope(&self, member_role: &str) -> Result<Option<Envelope>, StoreError> {
        let row = sqlx::query(
            "SELECT revision, spec \
             FROM envelopes \
             WHERE scope_kind = 'member_role' AND scope_ref = $1 \
             ORDER BY revision DESC \
             LIMIT 1",
        )
        .bind(member_role)
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
        let decision_id = Uuid::new_v4();
        let approval_id = Uuid::new_v4();
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let inserted = sqlx::query(
            "INSERT INTO admission_decisions \
             (id, runtime_uid, spec_digest, envelope_rev, verdict, deltas, proposed_spec, actor, \
              member_role, base_spec_digest) \
             VALUES ($1, $2, $3, $4, 'reject', $5, $6, $7, $8, $9) \
             ON CONFLICT (runtime_uid, spec_digest, envelope_rev, base_spec_digest, actor, member_role) \
             DO NOTHING \
             RETURNING id",
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
        .fetch_optional(&mut *transaction)
        .await
        .map_err(database_error)?;
        let parked = if inserted.is_some() {
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
            ParkedAdmission {
                decision_id,
                approval_id,
            }
        } else {
            let row = sqlx::query(
                "SELECT admission_decisions.id AS decision_id, approvals.id AS approval_id \
                 FROM admission_decisions \
                 JOIN approvals ON approvals.admission_decision_id = admission_decisions.id \
                 WHERE admission_decisions.runtime_uid = $1 \
                   AND admission_decisions.spec_digest = $2 \
                   AND admission_decisions.envelope_rev = $3 \
                   AND admission_decisions.base_spec_digest = $4 \
                   AND admission_decisions.actor = $5 \
                   AND admission_decisions.member_role = $6",
            )
            .bind(request.runtime_uid)
            .bind(request.spec_digest)
            .bind(request.envelope_revision)
            .bind(request.base_spec_digest)
            .bind(request.actor)
            .bind(request.member_role)
            .fetch_one(&mut *transaction)
            .await
            .map_err(database_error)?;
            ParkedAdmission {
                decision_id: row.try_get("decision_id").map_err(database_error)?,
                approval_id: row.try_get("approval_id").map_err(database_error)?,
            }
        };
        transaction.commit().await.map_err(database_error)?;
        Ok(parked)
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
                    envelope_revision: row.try_get("envelope_rev").map_err(database_error)?,
                    actor: row.try_get("actor").map_err(database_error)?,
                    member_role: row.try_get("member_role").map_err(database_error)?,
                })
            })
            .collect()
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
        if state != "pending" {
            return Err(StoreError::ApprovalNotPending);
        }
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
        } else {
            Err(StoreError::DecisionReferenceMismatch)
        }
    }

    pub async fn grants_for_runtime(
        &self,
        runtime_uid: &str,
        envelope_revision: i64,
    ) -> Result<Vec<AdmissionDelta>, StoreError> {
        let rows = sqlx::query(
            "SELECT grants.granted_value \
             FROM grants \
             LEFT JOIN grant_revocations ON grant_revocations.grant_id = grants.id \
             WHERE grants.runtime_uid = $1 \
               AND grants.envelope_revision = $2 \
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
                admission_decisions.base_spec_digest \
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
        })
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
        let member_role = row.try_get("member_role").map_err(database_error)?;
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
        let valid_expiry =
            sqlx::query_scalar::<_, bool>("SELECT (($1::text)::timestamptz > now())")
                .bind(request.expires_at)
                .fetch_one(&mut *transaction)
                .await
                .map_err(|_| StoreError::InvalidGrantExpiry)?;
        if !valid_expiry {
            return Err(StoreError::InvalidGrantExpiry);
        }

        sqlx::query(
            "UPDATE approvals \
             SET state = 'approved', \
                 decided_by = $1, \
                 decided_at = now(), \
                 rationale = $2 \
             WHERE id = $3",
        )
        .bind(request.decided_by)
        .bind(request.rationale)
        .bind(request.approval_id)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;
        for grant in &grants {
            sqlx::query(
                "INSERT INTO grants \
                 (id, runtime_uid, dimension, granted_value, approval_id, envelope_revision, expires_at) \
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

pub struct ParkRejection<'a> {
    pub runtime_uid: &'a str,
    pub spec_digest: &'a str,
    pub base_spec_digest: &'a str,
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
    ApprovalNotFound,
    ApprovalNotPending,
    MissingDecisionReference,
    DecisionReferenceMismatch,
    EvidenceMismatch,
    InvalidGrantExpiry,
    MissingRevocationReason,
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(reason) => write!(formatter, "Postgres operation failed: {reason}"),
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
        }
    }
}

impl Error for StoreError {}

fn database_error(error: sqlx::Error) -> StoreError {
    StoreError::Database(error.to_string())
}

fn grant_dimension(delta: &AdmissionDelta) -> &'static str {
    match delta {
        AdmissionDelta::Budget { .. } => "budget",
        AdmissionDelta::Ttl { .. } => "ttl",
        AdmissionDelta::Models { .. } => "models",
        AdmissionDelta::Tools { .. } => "tools",
    }
}
