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
        sqlx::query(
            "INSERT INTO admission_decisions \
             (id, runtime_uid, spec_digest, envelope_rev, verdict, deltas, proposed_spec, actor, member_role) \
             VALUES ($1, $2, $3, $4, 'reject', $5, $6, $7, $8)",
        )
        .bind(decision_id)
        .bind(request.runtime_uid)
        .bind(request.spec_digest)
        .bind(request.envelope_revision)
        .bind(Json(request.deltas))
        .bind(Json(request.proposed_spec))
        .bind(request.actor)
        .bind(request.member_role)
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
        })
    }

    pub async fn pending_approvals(&self) -> Result<Vec<PendingApproval>, StoreError> {
        let rows = sqlx::query(
            "SELECT \
                approvals.id AS approval_id, \
                admission_decisions.id AS decision_id, \
                approvals.runtime_uid, \
                admission_decisions.deltas, \
                admission_decisions.proposed_spec, \
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
                    deltas,
                    proposed_spec,
                    actor: row.try_get("actor").map_err(database_error)?,
                    member_role: row.try_get("member_role").map_err(database_error)?,
                })
            })
            .collect()
    }
}

pub struct ParkRejection<'a> {
    pub runtime_uid: &'a str,
    pub spec_digest: &'a str,
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
    pub deltas: Vec<AdmissionDelta>,
    pub proposed_spec: AgentRuntimeSpec,
    pub actor: String,
    pub member_role: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoreError {
    Database(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(reason) => write!(formatter, "Postgres operation failed: {reason}"),
        }
    }
}

impl Error for StoreError {}

fn database_error(error: sqlx::Error) -> StoreError {
    StoreError::Database(error.to_string())
}
