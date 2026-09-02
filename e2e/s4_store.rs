use std::env;
use std::error::Error;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::Row;
use sqlx::postgres::PgPoolOptions;
use steward_admission::internal_authorities::steward_connections_v1;
use steward_admission::{AdmissionDelta, Envelope, EnvelopeScopeKind, EnvelopeSpec};
use steward_apiserver::governed_connections::{
    CONNECTIONS_AUTHORITY_DIGEST, CONNECTIONS_AUTHORITY_VERSION, CONNECTIONS_SERVICE,
    ConnectionExecutionBindings, ConnectionOperationKind as PlannedConnectionOperationKind,
    plan_connection_operation,
};
use steward_store::{
    AgentRunQuery, AgentRunTimelineKind, AgentRunTimelineProvenance, ApproveAdmission,
    BrowserRbacAssignment, BrowserRbacAssignmentAction, BrowserRbacAssignmentChange,
    ConnectionExecutionBindingSnapshot, ConnectionOAuthPhase, ConnectionOperationKind,
    ConnectionOperationReservation, ConnectionOperationReservationRequest,
    ConnectionOperationRetention, ConnectionOperationState, ParkRejection, PgStore, StoreError,
    TaskReservationRequest,
};
use steward_types::{
    AgentRuntimeSpec, AgentType, Budget, CanonicalAuthorityBinding, CanonicalUserId, Duration,
    Email, ModelRef, OrganizationId, OrganizationIdentity, OrganizationIdentityMigration,
    OrganizationIdentityPolicy, Principal, RunnerRequirements, RuntimeOwnership, SpendSummary,
    TaskPhase,
};

fn governed_connection_bindings() -> ConnectionExecutionBindings {
    ConnectionExecutionBindings {
        bridge_image_digest:
            "ghcr.io/example-org/steward-connections-bridge@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_owned(),
        mcp_gw_origin: "https://mcp-gw.example.test".to_owned(),
        mcp_gw_version: "0.3.2".to_owned(),
        namespace: "steward-test".to_owned(),
        runtime_class: "kata-qemu".to_owned(),
    }
}

fn connection_operation_retention() -> ConnectionOperationRetention {
    ConnectionOperationRetention {
        cache_ttl_seconds: 5,
        result_ttl_seconds: 30,
        oauth_lifetime_seconds: 630,
    }
}

async fn reserve_governed_connection(
    store: &PgStore,
    user_id: &CanonicalUserId,
    email: &Email,
    operation_kind: ConnectionOperationKind,
    allow_status_cache: bool,
    idempotency_identity: &str,
) -> Result<ConnectionOperationReservation, StoreError> {
    let planned_kind = match operation_kind {
        ConnectionOperationKind::Status => PlannedConnectionOperationKind::Status,
        ConnectionOperationKind::Start => PlannedConnectionOperationKind::Start,
        ConnectionOperationKind::Disconnect => PlannedConnectionOperationKind::Disconnect,
    };
    let plan =
        plan_connection_operation(user_id, email, planned_kind, governed_connection_bindings())
            .map_err(|_| StoreError::InvalidConnectionOperation)?;
    let operation_id = sqlx::types::Uuid::new_v4();
    let runtime_name = format!("conn-{}", operation_id.simple());
    let binding_snapshot = ConnectionExecutionBindingSnapshot {
        bridge_image_digest: plan.bindings.bridge_image_digest.clone(),
        mcp_gw_origin: plan.bindings.mcp_gw_origin.clone(),
        mcp_gw_version: plan.bindings.mcp_gw_version.clone(),
        namespace: plan.bindings.namespace.clone(),
        runtime_class: plan.bindings.runtime_class.clone(),
    };
    let task = TaskReservationRequest {
        idempotency_key: idempotency_identity,
        submitter_service: CONNECTIONS_SERVICE,
        acting_user: Some(email.as_str()),
        acting_user_id: Some(user_id.as_str()),
        owner: email.as_str(),
        owner_user_id: user_id.as_str(),
        workflow: "internal:steward-connections/v1",
        workflow_name: None,
        workflow_version: None,
        workflow_digest: None,
        user_envelope_instance_id: None,
        user_envelope_revision: None,
        user_envelope_digest: None,
        coding_agent_runtime: "connections-bridge",
        runtime_namespace: &binding_snapshot.namespace,
        runtime_name: &runtime_name,
        runtime_ownership: RuntimeOwnership::Provisioned,
        runtime_spec: &plan.spec,
        agent_command: &plan.command,
        envelope_revision: CONNECTIONS_AUTHORITY_VERSION,
    };
    store
        .reserve_connection_operation(&ConnectionOperationReservationRequest {
            operation_id,
            operation_kind,
            authority_id: CONNECTIONS_SERVICE,
            authority_version: CONNECTIONS_AUTHORITY_VERSION,
            authority_digest: CONNECTIONS_AUTHORITY_DIGEST,
            bindings: &binding_snapshot,
            idempotency_identity,
            response_deadline_seconds: steward_connections_v1::RESPONSE_DEADLINE_SECONDS,
            allow_status_cache,
            input_archive: &[1],
            task,
        })
        .await
}

fn google_identity(
    subject: impl AsRef<str>,
    email: impl AsRef<str>,
) -> Result<OrganizationIdentity, Box<dyn Error>> {
    google_identity_for(
        "example.com",
        OrganizationId::parse("org_example")?,
        subject,
        email,
    )
}

fn google_identity_for(
    hosted_domain: impl AsRef<str>,
    organization_id: OrganizationId,
    subject: impl AsRef<str>,
    email: impl AsRef<str>,
) -> Result<OrganizationIdentity, Box<dyn Error>> {
    let hosted_domain = hosted_domain.as_ref();
    Ok(OrganizationIdentityPolicy::new(
        "https://accounts.google.com",
        hosted_domain,
        organization_id,
    )?
    .validate(
        "https://accounts.google.com",
        subject.as_ref(),
        hosted_domain,
        email.as_ref(),
        true,
    )?)
}

fn proposed_spec() -> AgentRuntimeSpec {
    AgentRuntimeSpec {
        principal: Principal::User {
            acting_user: Email("alice@example.com".to_owned()),
        },
        owner: Email("alice@example.com".to_owned()),
        canonical_authority: None,
        agent_type: AgentType {
            name: "base".to_owned(),
        },
        llms: vec![ModelRef {
            provider: "provider-a".to_owned(),
            model: "model-a".to_owned(),
        }],
        tools: Vec::new(),
        budget: Budget {
            monthly_limit: "220.00".to_owned(),
            single_run_limit: None,
            currency: "USD".to_owned(),
        },
        ttl: Duration("24h".to_owned()),
        runner: RunnerRequirements::default(),
        bindings: None,
    }
}

#[tokio::test]
async fn canonical_identity_requires_exact_subject_mapping_and_explicit_reconnect()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the identity Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let organization = OrganizationId::parse("org_example")?;
    let email = Email::parse(format!("identity-{suffix}@example.com"))?;
    let google = google_identity(format!("google-subject-{suffix}"), email.as_str())?;

    let principal = store
        .register_canonical_identity(&google, "identity-admin")
        .await?;
    assert_eq!(
        store.resolve_canonical_identity(&google).await?,
        principal,
        "an exact reviewed Google issuer/subject/hosted-domain mapping must resolve"
    );
    assert_eq!(
        store
            .resolve_canonical_principal(&principal.user_id, &email)
            .await?,
        principal,
        "a trusted workload reference must resolve by opaque ID plus current display email"
    );
    assert_eq!(
        store
            .register_canonical_identity(&google, "identity-admin")
            .await?,
        principal,
        "exact repeat registration must be idempotent"
    );

    let changed_subject =
        google_identity(format!("different-google-subject-{suffix}"), email.as_str())?;
    assert_eq!(
        store
            .register_canonical_identity(&changed_subject, "identity-admin")
            .await,
        Err(StoreError::CanonicalIdentityAmbiguousEmail),
        "an email match must never silently adopt a different subject"
    );

    let renamed_email = Email::parse(format!("identity-renamed-{suffix}@example.com"))?;
    let renamed_google = google_identity(google.subject(), renamed_email.as_str())?;
    assert_eq!(
        store.resolve_canonical_identity(&renamed_google).await,
        Err(StoreError::CanonicalIdentityStale),
        "a changed email requires an explicit audited reconnect"
    );
    assert_eq!(
        store
            .resolve_canonical_principal(&principal.user_id, &renamed_email)
            .await,
        Err(StoreError::CanonicalIdentityStale),
        "a workload mapper cannot pair an existing user ID with an unreviewed email"
    );
    store
        .change_canonical_identity_email(
            &principal.user_id,
            &email,
            &renamed_email,
            "identity-admin",
        )
        .await?;
    assert_eq!(
        store
            .resolve_canonical_identity(&renamed_google)
            .await?
            .user_id,
        principal.user_id
    );
    assert_eq!(
        store
            .resolve_canonical_principal(&principal.user_id, &renamed_email)
            .await?
            .user_id,
        principal.user_id
    );

    let future_issuer = OrganizationIdentityMigration::new_reviewed(
        "https://login.example.test",
        format!("future-subject-{suffix}"),
        "example.com",
        organization,
        renamed_email,
    )?;
    let migrated = store
        .attach_canonical_identity_subject(&principal.user_id, &future_issuer, "identity-admin")
        .await?;
    assert_eq!(migrated.user_id, principal.user_id);
    assert_eq!(
        store
            .attach_canonical_identity_subject(
                &principal.user_id,
                &future_issuer,
                "identity-admin",
            )
            .await?,
        migrated,
        "retrying an exact attachment to the same canonical user must be idempotent"
    );
    assert_eq!(
        store
            .resolve_migrated_canonical_identity(&future_issuer)
            .await?
            .user_id,
        principal.user_id,
        "an explicitly reviewed issuer migration must preserve the opaque user ID"
    );
    Ok(())
}

#[tokio::test]
async fn browser_rbac_is_canonical_user_scoped_append_only_and_revocable()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the browser RBAC Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let user = store
        .register_canonical_identity(
            &google_identity(
                format!("browser-rbac-user-{suffix}"),
                format!("browser-rbac-user-{suffix}@example.com"),
            )?,
            "identity-admin",
        )
        .await?
        .user_id;
    let other_user = store
        .register_canonical_identity(
            &google_identity(
                format!("browser-rbac-other-{suffix}"),
                format!("browser-rbac-other-{suffix}@example.com"),
            )?,
            "identity-admin",
        )
        .await?
        .user_id;
    let administrator = BrowserRbacAssignment::Administrator;
    let engineer = BrowserRbacAssignment::MemberRole("engineer".to_owned());
    for assignment in [&administrator, &engineer] {
        store
            .append_browser_rbac_assignment(BrowserRbacAssignmentChange {
                user_id: &user,
                assignment,
                action: BrowserRbacAssignmentAction::Grant,
                actor: "rbac-operator",
            })
            .await?;
    }
    assert_eq!(
        store.browser_rbac_assignments(&user).await?.member_roles,
        ["engineer"],
        "the canonical user receives only their explicit member-role assignment"
    );
    assert!(store.browser_rbac_assignments(&user).await?.is_admin);
    assert_eq!(
        store.browser_rbac_assignments(&other_user).await?,
        Default::default(),
        "a role event for one canonical user cannot grant another user authority"
    );

    store
        .append_browser_rbac_assignment(BrowserRbacAssignmentChange {
            user_id: &user,
            assignment: &engineer,
            action: BrowserRbacAssignmentAction::Revoke,
            actor: "rbac-operator",
        })
        .await?;
    let after_revoke = store.browser_rbac_assignments(&user).await?;
    assert!(
        after_revoke.is_admin,
        "an unrelated administrator grant remains active"
    );
    assert!(after_revoke.member_roles.is_empty());
    let mutation = sqlx::query(
        "UPDATE browser_rbac_assignment_events SET actor = 'mutation-attempt' WHERE user_id = $1",
    )
    .bind(user.as_str())
    .execute(store.pool())
    .await;
    assert!(
        mutation.is_err(),
        "RBAC history must be revoked by an appended event, never modified in place"
    );
    Ok(())
}

#[tokio::test]
async fn canonical_external_subject_is_globally_unique_across_google_organizations()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the identity Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let shared_subject = format!("google-shared-subject-{suffix}");
    let first = google_identity_for(
        "example.com",
        OrganizationId::parse("org_example")?,
        &shared_subject,
        format!("first-{suffix}@example.com"),
    )?;
    let other_organization = google_identity_for(
        "other.example",
        OrganizationId::parse("org_other")?,
        &shared_subject,
        format!("second-{suffix}@other.example"),
    )?;

    let first_principal = store
        .register_canonical_identity(&first, "identity-admin")
        .await?;
    assert_eq!(
        store
            .register_canonical_identity(&other_organization, "identity-admin")
            .await,
        Err(StoreError::CanonicalIdentityConflict),
        "one Google (issuer, subject) pair must not identify people in two organizations"
    );
    assert_eq!(
        store.resolve_canonical_identity(&first).await?,
        first_principal
    );
    assert_eq!(
        store.resolve_canonical_identity(&other_organization).await,
        Err(StoreError::CanonicalIdentityNotFound)
    );
    let pair_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM canonical_identity_subjects WHERE issuer = $1 AND subject = $2",
    )
    .bind(first.issuer())
    .bind(first.subject())
    .fetch_one(store.pool())
    .await?;
    assert_eq!(
        pair_count, 1,
        "the external pair must have exactly one owner"
    );
    Ok(())
}

#[tokio::test]
async fn canonical_external_subject_cannot_be_attached_to_another_user()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the identity Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let shared_subject = format!("migrated-attach-subject-{suffix}");
    let first = google_identity_for(
        "example.com",
        OrganizationId::parse("org_example")?,
        format!("google-first-subject-{suffix}"),
        format!("first-attach-{suffix}@example.com"),
    )?;
    let second = google_identity_for(
        "other.example",
        OrganizationId::parse("org_other")?,
        format!("google-second-subject-{suffix}"),
        format!("second-attach-{suffix}@other.example"),
    )?;
    let first_principal = store
        .register_canonical_identity(&first, "identity-admin")
        .await?;
    let first_attachment = OrganizationIdentityMigration::new_reviewed(
        "https://login.example.test",
        &shared_subject,
        "example.com",
        OrganizationId::parse("org_example")?,
        Email::parse(format!("first-attach-{suffix}@example.com"))?,
    )?;
    store
        .attach_canonical_identity_subject(
            &first_principal.user_id,
            &first_attachment,
            "identity-admin",
        )
        .await?;
    let second_principal = store
        .register_canonical_identity(&second, "identity-admin")
        .await?;
    let conflicting_attachment = OrganizationIdentityMigration::new_reviewed(
        "https://login.example.test",
        &shared_subject,
        "other.example",
        OrganizationId::parse("org_other")?,
        Email::parse(format!("second-attach-{suffix}@other.example"))?,
    )?;

    assert_eq!(
        store
            .attach_canonical_identity_subject(
                &second_principal.user_id,
                &conflicting_attachment,
                "identity-admin",
            )
            .await,
        Err(StoreError::CanonicalIdentityConflict),
        "an attachment must not move an external pair to another canonical user"
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_exact_registration_converges_on_one_canonical_user()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the identity Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let identity = google_identity(
        format!("google-concurrent-subject-{suffix}"),
        format!("concurrent-{suffix}@example.com"),
    )?;

    let left_store = store.clone();
    let right_store = store.clone();
    let (left, right) = tokio::join!(
        left_store.register_canonical_identity(&identity, "identity-admin-left"),
        right_store.register_canonical_identity(&identity, "identity-admin-right"),
    );
    let left = left?;
    let right = right?;
    assert_eq!(
        left, right,
        "concurrent exact registrations must resolve to one canonical user"
    );
    let pair_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM canonical_identity_subjects WHERE issuer = $1 AND subject = $2",
    )
    .bind(identity.issuer())
    .bind(identity.subject())
    .fetch_one(store.pool())
    .await?;
    assert_eq!(pair_count, 1, "the external pair must be persisted once");
    Ok(())
}

#[tokio::test]
async fn alternative_issuer_migration_cannot_allocate_a_canonical_user()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the identity Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let migration = OrganizationIdentityMigration::new_reviewed(
        "https://login.example.test",
        format!("migration-only-subject-{suffix}"),
        "example.com",
        OrganizationId::parse("org_example")?,
        Email::parse(format!("migration-only-{suffix}@example.com"))?,
    )?;
    let missing_user =
        steward_types::CanonicalUserId::parse("usr_00000000000000000000000000000000")?;
    let unrelated_identity = google_identity(
        format!("unrelated-concurrent-subject-{suffix}"),
        format!("unrelated-concurrent-{suffix}@example.com"),
    )?;
    let migration_store = store.clone();
    let unrelated_store = store.clone();
    let (migration_result, unrelated_result) = tokio::join!(
        migration_store.attach_canonical_identity_subject(
            &missing_user,
            &migration,
            "identity-admin",
        ),
        unrelated_store.register_canonical_identity(&unrelated_identity, "identity-admin"),
    );

    assert_eq!(
        migration_result,
        Err(StoreError::CanonicalIdentityNotFound),
        "a migration can attach only to an existing canonical user"
    );
    let unrelated_principal = unrelated_result?;
    assert_eq!(
        store
            .resolve_canonical_identity(&unrelated_identity)
            .await?,
        unrelated_principal,
        "the unrelated concurrent registration must complete"
    );
    let migration_user_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM canonical_users \
         WHERE organization_id = $1 AND lower(display_email) = lower($2)",
    )
    .bind(migration.organization_id().as_str())
    .bind(migration.verified_email().as_str())
    .fetch_one(store.pool())
    .await?;
    let migration_subject_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM canonical_identity_subjects WHERE issuer = $1 AND subject = $2",
    )
    .bind(migration.issuer())
    .bind(migration.subject())
    .fetch_one(store.pool())
    .await?;
    assert_eq!(
        migration_user_count, 0,
        "migration must not allocate its requested user"
    );
    assert_eq!(
        migration_subject_count, 0,
        "migration must not allocate its requested external-subject mapping"
    );
    Ok(())
}

#[tokio::test]
async fn canonical_subject_row_rejects_and_detects_wrong_user_organization()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the identity Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let owner_identity = google_identity(
        format!("organization-owner-subject-{suffix}"),
        format!("organization-owner-{suffix}@example.com"),
    )?;
    let owner = store
        .register_canonical_identity(&owner_identity, "identity-admin")
        .await?;
    let wrong_organization_identity = google_identity_for(
        "other.example",
        OrganizationId::parse("org_other")?,
        format!("wrong-organization-subject-{suffix}"),
        format!("wrong-organization-{suffix}@other.example"),
    )?;
    let insert_wrong_organization = sqlx::query(
        "INSERT INTO canonical_identity_subjects \
         (issuer, subject, organization_claim, organization_id, user_id, verified_email) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(wrong_organization_identity.issuer())
    .bind(wrong_organization_identity.subject())
    .bind(wrong_organization_identity.organization_claim())
    .bind(wrong_organization_identity.organization_id().as_str())
    .bind(owner.user_id.as_str())
    .bind(wrong_organization_identity.verified_email().as_str())
    .execute(store.pool())
    .await;
    assert!(
        insert_wrong_organization.is_err(),
        "the database must reject a subject row whose organization differs from its user"
    );

    // Exercise the resolver defense independently of the FK by simulating a pre-constraint
    // corrupt row. This is test-only superuser state and is restored before the connection is
    // returned to the pool.
    let mut corrupt_row = store.pool().begin().await?;
    sqlx::query("SET LOCAL session_replication_role = replica")
        .execute(&mut *corrupt_row)
        .await?;
    sqlx::query(
        "INSERT INTO canonical_identity_subjects \
         (issuer, subject, organization_claim, organization_id, user_id, verified_email) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(wrong_organization_identity.issuer())
    .bind(wrong_organization_identity.subject())
    .bind(wrong_organization_identity.organization_claim())
    .bind(wrong_organization_identity.organization_id().as_str())
    .bind(owner.user_id.as_str())
    .bind(wrong_organization_identity.verified_email().as_str())
    .execute(&mut *corrupt_row)
    .await?;
    corrupt_row.commit().await?;

    assert_eq!(
        store
            .resolve_canonical_identity(&wrong_organization_identity)
            .await,
        Err(StoreError::CanonicalIdentityInvalidRecord),
        "resolution must fail closed even if a corrupt row bypassed the schema constraint"
    );
    Ok(())
}

#[tokio::test]
async fn task_submission_state_is_idempotent_durable_and_single_claimed()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the Task Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let idempotency_key = format!("job-{suffix}");
    let runtime_name = format!("task-{suffix}");
    let runtime_uid = format!("runtime-{suffix}");
    let canonical = store
        .register_canonical_identity(
            &google_identity(
                format!("google-subject-{suffix}"),
                format!("alice-{suffix}@example.com"),
            )?,
            "test-bootstrap",
        )
        .await?;
    let mut spec = proposed_spec();
    spec.canonical_authority = Some(CanonicalAuthorityBinding::new(
        canonical.user_id.clone(),
        Some(canonical.user_id.clone()),
    )?);
    let command = vec!["agent-v1".to_owned()];
    let legacy = sqlx::query(
        "INSERT INTO task_submissions \
         (task_uid, idempotency_key, submitter_service, acting_user, owner, workflow, \
          coding_agent_runtime, runtime_namespace, runtime_name, runtime_ownership, phase, \
          runtime_spec, agent_command) \
         VALUES (gen_random_uuid(), $1, 'legacy-service', 'legacy@example.com', \
                 'legacy@example.com', 'legacy-workflow', 'legacy-runtime', 'legacy', $2, \
                 'provisioned', 'submitted', '{}'::jsonb, '[]'::jsonb) \
         RETURNING identity_binding_state, acting_user_id, owner_user_id, runtime_spec",
    )
    .bind(format!("legacy-{suffix}"))
    .bind(format!("legacy-{suffix}"))
    .fetch_one(store.pool())
    .await?;
    assert_eq!(
        legacy.try_get::<String, _>("identity_binding_state")?,
        "legacy_reconnect_required"
    );
    assert_eq!(legacy.try_get::<Option<String>, _>("acting_user_id")?, None);
    assert_eq!(legacy.try_get::<Option<String>, _>("owner_user_id")?, None);
    assert_eq!(
        legacy
            .try_get::<serde_json::Value, _>("runtime_spec")?
            .get("canonicalAuthority"),
        None,
        "legacy rows must remain explicitly unbound instead of adopting an email-derived ID"
    );

    let request = TaskReservationRequest {
        idempotency_key: &idempotency_key,
        submitter_service: "steward-run",
        acting_user: Some("alice@example.com"),
        acting_user_id: Some(canonical.user_id.as_str()),
        owner: "alice@example.com",
        owner_user_id: canonical.user_id.as_str(),
        workflow: "code-review",
        workflow_name: None,
        workflow_version: None,
        workflow_digest: None,
        user_envelope_instance_id: None,
        user_envelope_revision: None,
        user_envelope_digest: None,
        coding_agent_runtime: "agent-v1",
        runtime_namespace: "team-a",
        runtime_name: &runtime_name,
        runtime_ownership: RuntimeOwnership::Provisioned,
        runtime_spec: &spec,
        agent_command: &command,
        envelope_revision: 1,
    };
    let first = store.reserve_task(&request).await?;
    assert!(first.inserted);
    assert_eq!(
        first.record.runtime_spec.canonical_authority, spec.canonical_authority,
        "the stable authority binding must survive Task persistence"
    );
    let second = store.reserve_task(&request).await?;
    assert!(!second.inserted);
    assert_eq!(second.record.task_uid, first.record.task_uid);

    let other = store
        .register_canonical_identity(
            &google_identity(
                format!("google-other-subject-{suffix}"),
                format!("bob-{suffix}@example.com"),
            )?,
            "test-bootstrap",
        )
        .await?;
    let mut other_spec = proposed_spec();
    other_spec.canonical_authority = Some(CanonicalAuthorityBinding::new(
        other.user_id.clone(),
        Some(other.user_id.clone()),
    )?);
    let other_runtime_name = format!("task-other-{suffix}");
    let other_request = TaskReservationRequest {
        acting_user_id: Some(other.user_id.as_str()),
        owner_user_id: other.user_id.as_str(),
        runtime_name: &other_runtime_name,
        runtime_spec: &other_spec,
        ..request
    };
    let other_task = store.reserve_task(&other_request).await?;
    assert!(other_task.inserted);
    assert_ne!(other_task.record.task_uid, first.record.task_uid);
    assert_ne!(other_task.record.runtime_name, first.record.runtime_name);
    let other_retry = store.reserve_task(&other_request).await?;
    assert!(!other_retry.inserted);
    assert_eq!(other_retry.record.task_uid, other_task.record.task_uid);

    let injected_runtime_name = format!("task-injected-{suffix}");
    let injected_same_owner = TaskReservationRequest {
        runtime_name: &injected_runtime_name,
        ..request
    };
    assert_eq!(
        store.reserve_task(&injected_same_owner).await,
        Err(StoreError::TaskIdempotencyConflict),
        "a retry cannot adopt an injected or legacy runtime name outside its durable reservation"
    );
    assert!(
        store
            .task_for_submitter(first.record.task_uid, "steward-run", other.user_id.as_str(),)
            .await?
            .is_none(),
        "a different canonical owner must not observe the first owner's Task"
    );
    assert_eq!(
        store
            .request_task_finalization(
                first.record.task_uid,
                "steward-run",
                other.user_id.as_str(),
            )
            .await,
        Err(StoreError::TaskNotFound),
        "a different canonical owner must not delete the first owner's runtime through Task finalization"
    );

    let legacy_key = format!("legacy-{suffix}");
    let rebound_runtime_name = format!("task-reconnected-{suffix}");
    let legacy_reconnect = TaskReservationRequest {
        idempotency_key: &legacy_key,
        submitter_service: "legacy-service",
        runtime_name: &rebound_runtime_name,
        ..request
    };
    let reconnected = store.reserve_task(&legacy_reconnect).await?;
    assert!(reconnected.inserted);
    assert_eq!(reconnected.record.runtime_name, rebound_runtime_name);
    assert_ne!(
        reconnected.record.identity_binding_state, "legacy_reconnect_required",
        "a canonical reconnect creates a new bound row instead of adopting the legacy row"
    );

    let mismatched_columns_key = format!("mismatched-columns-{suffix}");
    let mismatched_columns = TaskReservationRequest {
        idempotency_key: &mismatched_columns_key,
        acting_user_id: Some(other.user_id.as_str()),
        ..request
    };
    assert_eq!(
        store.reserve_task(&mismatched_columns).await,
        Err(StoreError::InvalidTaskIdentityBinding),
        "delegated v1 reservations must reject acting_user_id != owner_user_id"
    );

    let mismatched_runtime_key = format!("mismatched-runtime-{suffix}");
    let mismatched_runtime_authority = TaskReservationRequest {
        idempotency_key: &mismatched_runtime_key,
        owner_user_id: other.user_id.as_str(),
        acting_user_id: Some(other.user_id.as_str()),
        ..request
    };
    assert_eq!(
        store.reserve_task(&mismatched_runtime_authority).await,
        Err(StoreError::InvalidTaskIdentityBinding),
        "Task columns must match the server-authored runtime canonical authority"
    );

    let direct_mismatch = sqlx::query(
        "INSERT INTO task_submissions \
         (task_uid, idempotency_key, submitter_service, acting_user, acting_user_id, owner, \
          owner_user_id, identity_binding_state, workflow, coding_agent_runtime, \
          runtime_namespace, runtime_name, runtime_ownership, phase, runtime_spec, agent_command) \
         VALUES (gen_random_uuid(), $1, 'steward-run', 'alice@example.com', $2, \
                 'alice@example.com', $3, 'bound', 'code-review', 'agent-v1', 'team-a', $4, \
                 'provisioned', 'submitted', $5, '[]'::jsonb)",
    )
    .bind(format!("direct-mismatch-{suffix}"))
    .bind(other.user_id.as_str())
    .bind(canonical.user_id.as_str())
    .bind(format!("direct-mismatch-{suffix}"))
    .bind(sqlx::types::Json(&spec))
    .execute(store.pool())
    .await;
    assert!(
        direct_mismatch.is_err(),
        "the database must reject delegated acting_user_id != owner_user_id"
    );

    store
        .bind_task_runtime(first.record.task_uid, &runtime_uid, TaskPhase::Submitted)
        .await?;
    store
        .record_spend_observation(
            &runtime_uid,
            1,
            "task-read-model-spec",
            &SpendSummary {
                observed_amount: "1.25".to_owned(),
                currency: "USD".to_owned(),
            },
            false,
        )
        .await?;
    let archive = b"neutral-tar-fixture";
    store
        .put_task_inputs(
            first.record.task_uid,
            "steward-run",
            canonical.user_id.as_str(),
            archive,
        )
        .await?;
    store
        .request_task_execution(
            first.record.task_uid,
            "steward-run",
            canonical.user_id.as_str(),
        )
        .await?;
    assert!(store.claim_task_execution(first.record.task_uid).await?);
    assert!(
        !store.claim_task_execution(first.record.task_uid).await?,
        "a task execution must have only one durable winner"
    );
    store
        .complete_task_execution(first.record.task_uid, b"neutral-output-tar")
        .await?;
    let completed = store
        .task(first.record.task_uid)
        .await?
        .ok_or_else(|| io::Error::other("completed task disappeared"))?;
    assert_eq!(completed.phase, TaskPhase::Succeeded);
    assert_eq!(
        completed.output_archive.as_deref(),
        Some(b"neutral-output-tar".as_slice())
    );
    store
        .request_task_finalization(
            first.record.task_uid,
            "steward-run",
            canonical.user_id.as_str(),
        )
        .await?;
    store.mark_task_finalized(first.record.task_uid).await?;
    assert!(
        store
            .task(first.record.task_uid)
            .await?
            .ok_or_else(|| io::Error::other("finalized task disappeared"))?
            .finalized
    );
    let page = store
        .agent_runs(&AgentRunQuery {
            limit: 10,
            cursor: None,
            phase: Some(TaskPhase::Succeeded),
            workflow: Some("code-review".to_owned()),
            owner_user_id: None,
            runtime_uid: None,
            user_envelope_instance_id: None,
            task_uid: None,
        })
        .await?;
    let read_model = page
        .records
        .iter()
        .find(|record| record.task_uid == first.record.task_uid)
        .ok_or_else(|| io::Error::other("completed task is absent from Agent Runs"))?;
    assert_eq!(read_model.envelope_revision, Some(1));
    assert_eq!(read_model.runtime_spec, spec);
    assert_eq!(
        read_model
            .spend
            .as_ref()
            .map(|spend| spend.observed_amount.as_str()),
        Some("1.25")
    );
    assert!(!read_model.history_partial);
    let owner_scoped = store
        .agent_runs(&AgentRunQuery {
            limit: 10,
            cursor: None,
            phase: Some(TaskPhase::Succeeded),
            workflow: Some("code-review".to_owned()),
            owner_user_id: Some(canonical.user_id.as_str().to_owned()),
            runtime_uid: None,
            user_envelope_instance_id: None,
            task_uid: None,
        })
        .await?;
    assert!(
        owner_scoped
            .records
            .iter()
            .any(|record| record.task_uid == first.record.task_uid),
        "the canonical owner scope must return the caller's run"
    );
    assert_eq!(
        store
            .agent_runs(&AgentRunQuery {
                limit: 10,
                cursor: Some(first.record.task_uid),
                phase: None,
                workflow: None,
                owner_user_id: Some(other.user_id.as_str().to_owned()),
                runtime_uid: None,
                user_envelope_instance_id: None,
                task_uid: None,
            })
            .await,
        Err(StoreError::InvalidRunCursor),
        "an owner-scoped cursor must not reveal another user's run boundary"
    );
    let timeline = store
        .agent_run_timeline(first.record.task_uid)
        .await?
        .ok_or_else(|| io::Error::other("completed task timeline disappeared"))?;
    assert!(
        timeline
            .iter()
            .all(|event| { event.provenance == AgentRunTimelineProvenance::Recorded })
    );
    assert!(
        timeline
            .iter()
            .any(|event| { event.kind == AgentRunTimelineKind::Phase(TaskPhase::Succeeded) })
    );
    assert!(
        timeline
            .iter()
            .any(|event| { event.kind == AgentRunTimelineKind::FinalizationRequested })
    );
    assert!(
        timeline
            .iter()
            .any(|event| { event.kind == AgentRunTimelineKind::Finalized })
    );
    Ok(())
}

fn budget_deltas() -> Vec<AdmissionDelta> {
    vec![AdmissionDelta::Budget {
        requested: "220.00".to_owned(),
        ceiling: "200.00".to_owned(),
        currency: "USD".to_owned(),
    }]
}

fn base_spec() -> AgentRuntimeSpec {
    let mut spec = proposed_spec();
    spec.budget.monthly_limit = "100.00".to_owned();
    spec
}

fn envelope(member_limit: &str, revision: i64) -> Envelope {
    let spec = proposed_spec();
    Envelope {
        revision,
        spec: EnvelopeSpec {
            llms: spec.llms,
            tools: spec.tools,
            budget: Budget {
                monthly_limit: member_limit.to_owned(),
                single_run_limit: None,
                currency: spec.budget.currency,
            },
            ttl: spec.ttl,
            runner: RunnerRequirements::default(),
        },
    }
}

#[tokio::test]
async fn s4_service_envelopes_and_grants_are_isolated_from_equal_role_names()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the S4 Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let scope_ref = format!("shared-scope-{suffix}");
    let runtime_uid = format!("runtime-service-{suffix}");

    store
        .insert_envelope(&scope_ref, &envelope("200.00", 1), "admin@example.com")
        .await?;
    store
        .insert_service_envelope(&scope_ref, &envelope("50.00", 1), "admin@example.com")
        .await?;
    assert_eq!(
        store
            .latest_envelope(&scope_ref)
            .await?
            .ok_or_else(|| io::Error::other("member-role envelope disappeared"))?
            .spec
            .budget
            .monthly_limit,
        "200.00"
    );
    assert_eq!(
        store
            .latest_service_envelope(&scope_ref)
            .await?
            .ok_or_else(|| io::Error::other("service envelope disappeared"))?
            .spec
            .budget
            .monthly_limit,
        "50.00"
    );

    let mut proposed = proposed_spec();
    proposed.principal = Principal::Service {
        name: scope_ref.clone(),
        acting_user: None,
    };
    proposed.owner = Email("alice@example.com".to_owned());
    proposed.budget.monthly_limit = "60.00".to_owned();
    let mut base = proposed.clone();
    base.budget.monthly_limit = "0.00".to_owned();
    let deltas = vec![AdmissionDelta::Budget {
        requested: "60.00".to_owned(),
        ceiling: "50.00".to_owned(),
        currency: "USD".to_owned(),
    }];
    let parked = store
        .park_rejection(ParkRejection {
            runtime_uid: &runtime_uid,
            runtime_namespace: "team-a",
            runtime_name: &runtime_uid,
            spec_digest: "service-proposed-digest",
            base_spec_digest: "service-base-digest",
            base_pending_approval_digest: None,
            base_spec: &base,
            envelope_revision: 1,
            deltas: &deltas,
            proposed_spec: &proposed,
            actor: &scope_ref,
            member_role: &scope_ref,
        })
        .await?;
    store
        .link_decision_reference(
            parked.approval_id,
            "PROJ-123",
            "https://jira.example.com/browse/PROJ-123",
        )
        .await?;
    store
        .approve_admission(ApproveAdmission {
            approval_id: parked.approval_id,
            decided_by: "admin@example.com",
            rationale: "approve one service runtime",
            evidence_url: "https://jira.example.com/browse/PROJ-123",
            expires_at: "2999-01-01T00:00:00Z",
        })
        .await?;

    assert_eq!(
        store
            .grants_for_runtime_scoped(&runtime_uid, EnvelopeScopeKind::Service, &scope_ref, 1,)
            .await?,
        deltas
    );
    assert!(
        store
            .grants_for_runtime(&runtime_uid, &scope_ref, 1)
            .await?
            .is_empty(),
        "an equal member-role scope must not inherit the service grant"
    );

    store
        .insert_envelope(&scope_ref, &envelope("250.00", 2), "admin@example.com")
        .await?;
    assert_eq!(
        store
            .grants_for_runtime_scoped(&runtime_uid, EnvelopeScopeKind::Service, &scope_ref, 1,)
            .await?,
        deltas,
        "a role-envelope revision must not revoke an equal-named service grant"
    );
    store
        .insert_service_envelope(&scope_ref, &envelope("75.00", 2), "admin@example.com")
        .await?;
    assert!(
        store
            .grants_for_runtime_scoped(&runtime_uid, EnvelopeScopeKind::Service, &scope_ref, 1,)
            .await?
            .is_empty(),
        "a new service-envelope revision must revoke the stale service grant"
    );
    Ok(())
}

#[tokio::test]
async fn s4_grants_are_append_only_and_bound_to_one_runtime() -> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the S4 Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;

    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let runtime_a = format!("runtime-a-{suffix}");
    let runtime_b = format!("runtime-b-{suffix}");
    let decision_id = sqlx::query("SELECT gen_random_uuid()::text AS id")
        .fetch_one(store.pool())
        .await?
        .try_get::<String, _>("id")?;
    let approval_id = sqlx::query("SELECT gen_random_uuid()::text AS id")
        .fetch_one(store.pool())
        .await?
        .try_get::<String, _>("id")?;
    let grant_id = sqlx::query("SELECT gen_random_uuid()::text AS id")
        .fetch_one(store.pool())
        .await?
        .try_get::<String, _>("id")?;

    sqlx::query(
        "INSERT INTO admission_decisions \
         (id, runtime_uid, spec_digest, envelope_rev, verdict, deltas, proposed_spec, actor, \
          member_role, base_spec_digest, base_spec, runtime_namespace, runtime_name) \
         VALUES ($1::uuid, $2, 'digest-a', 1, 'reject', '[]'::jsonb, $3::jsonb, \
                 'alice@example.com', 'engineer', 'base-digest-a', $4::jsonb, \
                 'team-a', 'runtime-a')",
    )
    .bind(&decision_id)
    .bind(&runtime_a)
    .bind(serde_json::to_value(proposed_spec())?)
    .bind(serde_json::to_value(base_spec())?)
    .execute(store.pool())
    .await?;
    sqlx::query(
        "INSERT INTO approvals \
         (id, runtime_uid, admission_decision_id, state, decision_key) \
         VALUES ($1::uuid, $2, $3::uuid, 'pending', 'PROJ-123')",
    )
    .bind(&approval_id)
    .bind(&runtime_a)
    .bind(&decision_id)
    .execute(store.pool())
    .await?;

    let inserted = sqlx::query(
        "INSERT INTO grants \
         (id, runtime_uid, dimension, granted_value, approval_id, envelope_revision, expires_at) \
         VALUES ($1::uuid, $2, 'budget', \
                 '{\"dimension\":\"budget\",\"requested\":\"220.00\",\
                   \"ceiling\":\"200.00\",\"currency\":\"USD\"}'::jsonb, \
                 $3::uuid, 1, '2999-01-01T00:00:00Z')",
    )
    .bind(&grant_id)
    .bind(&runtime_a)
    .bind(&approval_id)
    .execute(store.pool())
    .await;
    assert!(
        inserted.is_ok(),
        "S4 must persist an instance-bound grant row before provisioning: {inserted:?}"
    );

    let rebound = sqlx::query("UPDATE grants SET runtime_uid = $1 WHERE id = $2::uuid")
        .bind(&runtime_b)
        .bind(&grant_id)
        .execute(store.pool())
        .await;
    assert!(
        rebound.is_err(),
        "a granted exception must never be rebound to a second runtime UID"
    );
    let visible_to_second_runtime =
        sqlx::query("SELECT id FROM grants WHERE runtime_uid = $1 AND id = $2::uuid")
            .bind(&runtime_b)
            .bind(&grant_id)
            .fetch_optional(store.pool())
            .await?;
    assert!(
        visible_to_second_runtime.is_none(),
        "a second runtime must not observe the first runtime's grant"
    );

    let deleted = sqlx::query("DELETE FROM grants WHERE id = $1::uuid")
        .bind(&grant_id)
        .execute(store.pool())
        .await;
    assert!(
        deleted.is_err(),
        "grant history must be append-only so approval evidence cannot disappear"
    );
    Ok(())
}

#[tokio::test]
async fn s4_repeated_parking_reuses_one_approval_and_one_channel_marker()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the S4 Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let proposed_spec = proposed_spec();
    let base_spec = base_spec();
    let deltas = budget_deltas();
    let request = || ParkRejection {
        runtime_uid: "runtime-retry-a",
        runtime_namespace: "team-a",
        runtime_name: "runtime-retry-a",
        spec_digest: "digest-retry-a",
        base_spec_digest: "base-digest-retry-a",
        base_pending_approval_digest: None,
        base_spec: &base_spec,
        envelope_revision: 1,
        deltas: &deltas,
        proposed_spec: &proposed_spec,
        actor: "alice@example.com",
        member_role: "engineer",
    };

    let first = store.park_rejection(request()).await?;
    let second = store.park_rejection(request()).await?;
    assert_eq!(
        first, second,
        "retrying the same rejected manifest must reuse its approval so a failed channel request can be retried"
    );
    let decision_count = sqlx::query(
        "SELECT count(*)::bigint AS count \
         FROM admission_decisions \
         WHERE runtime_uid = 'runtime-retry-a' AND spec_digest = 'digest-retry-a'",
    )
    .fetch_one(store.pool())
    .await?
    .try_get::<i64, _>("count")?;
    assert_eq!(decision_count, 1);
    Ok(())
}

#[tokio::test]
async fn s4_active_grants_expire_and_can_be_revoked_without_erasing_history()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the S4 Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let proposed_spec = proposed_spec();
    let base_spec = base_spec();
    let deltas = budget_deltas();
    store
        .insert_envelope(
            "engineer-revocation",
            &envelope("200.00", 7),
            "admin@example.com",
        )
        .await?;
    let parked = store
        .park_rejection(ParkRejection {
            runtime_uid: "runtime-revocation-a",
            runtime_namespace: "team-a",
            runtime_name: "runtime-revocation-a",
            spec_digest: "digest-revocation-a",
            base_spec_digest: "base-digest-revocation-a",
            base_pending_approval_digest: None,
            base_spec: &base_spec,
            envelope_revision: 7,
            deltas: &deltas,
            proposed_spec: &proposed_spec,
            actor: "alice@example.com",
            member_role: "engineer-revocation",
        })
        .await?;
    store
        .link_decision_reference(
            parked.approval_id,
            "PROJ-123",
            "https://jira.example.com/browse/PROJ-123",
        )
        .await?;
    let expired = store
        .approve_admission(ApproveAdmission {
            approval_id: parked.approval_id,
            decided_by: "admin@example.com",
            rationale: "unbounded exception attempt",
            evidence_url: "https://jira.example.com/browse/PROJ-123",
            expires_at: "2000-01-01T00:00:00Z",
        })
        .await;
    assert_eq!(
        expired,
        Err(StoreError::InvalidGrantExpiry),
        "an approval must not create authority with an absent or elapsed lifetime",
    );
    store
        .approve_admission(ApproveAdmission {
            approval_id: parked.approval_id,
            decided_by: "admin@example.com",
            rationale: "bounded exception",
            evidence_url: "https://jira.example.com/browse/PROJ-123",
            expires_at: "2999-01-01T00:00:00Z",
        })
        .await?;

    let context = sqlx::query(
        "SELECT envelope_revision, expires_at IS NOT NULL AS expires \
         FROM grants WHERE approval_id = $1",
    )
    .bind(parked.approval_id)
    .fetch_one(store.pool())
    .await?;
    assert_eq!(context.try_get::<i64, _>("envelope_revision")?, 7);
    assert!(context.try_get::<bool, _>("expires")?);
    assert!(
        store
            .grants_for_runtime("runtime-revocation-a", "engineer-revocation", 8)
            .await?
            .is_empty(),
        "a grant must not survive a change from the envelope revision it approved",
    );
    assert_eq!(
        store
            .revoke_runtime_grants(
                "runtime-revocation-a",
                "admin@example.com",
                "scope narrowed",
            )
            .await?,
        1,
    );
    assert!(
        store
            .grants_for_runtime("runtime-revocation-a", "engineer-revocation", 7)
            .await?
            .is_empty(),
        "an append-only revocation must remove the grant from active authority"
    );
    let retained =
        sqlx::query("SELECT count(*)::bigint AS count FROM grants WHERE approval_id = $1")
            .bind(parked.approval_id)
            .fetch_one(store.pool())
            .await?
            .try_get::<i64, _>("count")?;
    assert_eq!(
        retained, 1,
        "revocation must retain immutable grant evidence"
    );
    Ok(())
}

#[tokio::test]
async fn s4_application_requires_every_granted_dimension_to_remain_active()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the S4 Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let runtime_uid = format!("runtime-multigrant-{suffix}");
    let member_role = format!("engineer-multigrant-{suffix}");
    let mut proposed = proposed_spec();
    proposed.ttl = Duration("48h".to_owned());
    let base = base_spec();
    let deltas = vec![
        AdmissionDelta::Budget {
            requested: "220.00".to_owned(),
            ceiling: "200.00".to_owned(),
            currency: "USD".to_owned(),
        },
        AdmissionDelta::Ttl {
            requested: "48h".to_owned(),
            ceiling: "24h".to_owned(),
        },
    ];
    store
        .insert_envelope(&member_role, &envelope("200.00", 1), "admin@example.com")
        .await?;
    let parked = store
        .park_rejection(ParkRejection {
            runtime_uid: &runtime_uid,
            runtime_namespace: "team-a",
            runtime_name: "runtime-multigrant",
            spec_digest: &format!("digest-{suffix}"),
            base_spec_digest: &format!("base-digest-{suffix}"),
            base_pending_approval_digest: None,
            base_spec: &base,
            envelope_revision: 1,
            deltas: &deltas,
            proposed_spec: &proposed,
            actor: "alice@example.com",
            member_role: &member_role,
        })
        .await?;
    store
        .link_decision_reference(
            parked.approval_id,
            "PROJ-123",
            "https://jira.example.com/browse/PROJ-123",
        )
        .await?;
    store
        .approve_admission(ApproveAdmission {
            approval_id: parked.approval_id,
            decided_by: "admin@example.com",
            rationale: "bounded multi-dimension exception",
            evidence_url: "https://jira.example.com/browse/PROJ-123",
            expires_at: "2999-01-01T00:00:00Z",
        })
        .await?;
    assert!(store.grant_application(&runtime_uid).await?.is_some());
    let revoked_grant = sqlx::query_scalar::<_, String>(
        "SELECT id::text FROM grants WHERE approval_id = $1 AND dimension = 'budget'",
    )
    .bind(parked.approval_id)
    .fetch_one(store.pool())
    .await?;
    sqlx::query(
        "INSERT INTO grant_revocations (grant_id, revoked_by, reason) \
         VALUES (($1::text)::uuid, $2, $3)",
    )
    .bind(revoked_grant)
    .bind("admin@example.com")
    .bind("one dimension revoked")
    .execute(store.pool())
    .await?;
    assert!(
        store.grant_application(&runtime_uid).await?.is_none(),
        "a partially revoked approval must not restore its complete proposed spec"
    );
    assert!(
        store.grant_reversion(&runtime_uid).await?.is_some(),
        "partial revocation must schedule restoration of the pre-grant spec"
    );
    Ok(())
}

#[tokio::test]
async fn s4_approval_rejects_evidence_not_bound_to_the_parked_issue() -> Result<(), Box<dyn Error>>
{
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the S4 Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;

    let proposed_spec = AgentRuntimeSpec {
        principal: Principal::User {
            acting_user: Email("alice@example.com".to_owned()),
        },
        owner: Email("alice@example.com".to_owned()),
        canonical_authority: None,
        agent_type: AgentType {
            name: "base".to_owned(),
        },
        llms: vec![ModelRef {
            provider: "provider-a".to_owned(),
            model: "model-a".to_owned(),
        }],
        tools: Vec::new(),
        budget: Budget {
            monthly_limit: "220.00".to_owned(),
            single_run_limit: None,
            currency: "USD".to_owned(),
        },
        ttl: Duration("24h".to_owned()),
        runner: RunnerRequirements::default(),
        bindings: None,
    };
    let deltas = vec![AdmissionDelta::Budget {
        requested: "220.00".to_owned(),
        ceiling: "200.00".to_owned(),
        currency: "USD".to_owned(),
    }];
    let mut base_spec = proposed_spec.clone();
    base_spec.budget.monthly_limit = "100.00".to_owned();
    store
        .insert_envelope(
            "engineer-evidence",
            &envelope("200.00", 1),
            "admin@example.com",
        )
        .await?;
    assert_eq!(
        store
            .insert_envelope(
                "engineer-evidence",
                &envelope("200.00", 1),
                "admin@example.com",
            )
            .await,
        Err(StoreError::EnvelopeRevisionNotIncreasing),
        "an older revision must not become the event that invalidates current grants"
    );
    let parked = store
        .park_rejection(ParkRejection {
            runtime_uid: "runtime-evidence-a",
            runtime_namespace: "team-a",
            runtime_name: "runtime-evidence-a",
            spec_digest: "digest-evidence-a",
            base_spec_digest: "base-digest-evidence-a",
            base_pending_approval_digest: None,
            base_spec: &base_spec,
            envelope_revision: 1,
            deltas: &deltas,
            proposed_spec: &proposed_spec,
            actor: "alice@example.com",
            member_role: "engineer-evidence",
        })
        .await?;
    store
        .link_decision_reference(
            parked.approval_id,
            "PROJ-123",
            "https://jira.example.com/browse/PROJ-123",
        )
        .await
        .map_err(|error| {
            io::Error::other(format!(
                "the Jira reference must bind to the parked approval: {error}"
            ))
        })?;

    let result = store
        .approve_admission(ApproveAdmission {
            approval_id: parked.approval_id,
            decided_by: "admin@example.com",
            rationale: "approved for this runtime",
            evidence_url: "https://jira.example.com/browse/PROJ-999",
            expires_at: "2999-01-01T00:00:00Z",
        })
        .await;
    assert_eq!(
        result,
        Err(StoreError::EvidenceMismatch),
        "Steward must reject approval evidence that is not the parked request's Jira link"
    );

    let approved = store
        .approve_admission(ApproveAdmission {
            approval_id: parked.approval_id,
            decided_by: "admin@example.com",
            rationale: "approved for this runtime",
            evidence_url: "https://jira.example.com/browse/PROJ-123",
            expires_at: "2999-01-01T00:00:00Z",
        })
        .await;
    let approved = approved.map_err(|error| {
        io::Error::other(format!(
            "correctly bound evidence must approve the parked request: {error}"
        ))
    })?;
    assert_eq!(approved.approval_id, parked.approval_id);
    assert_eq!(approved.decision_id, parked.decision_id);
    assert_eq!(approved.runtime_uid, "runtime-evidence-a");
    assert_eq!(approved.proposed_spec, proposed_spec);
    assert_eq!(approved.actor, "alice@example.com");
    assert_eq!(approved.member_role, "engineer-evidence");
    assert_eq!(approved.decision_key, "PROJ-123");
    assert_eq!(
        approved.evidence_url,
        "https://jira.example.com/browse/PROJ-123"
    );
    assert_eq!(approved.grants, deltas);

    let approval_state =
        sqlx::query("SELECT state, decided_by, rationale FROM approvals WHERE id = $1")
            .bind(parked.approval_id)
            .fetch_one(store.pool())
            .await?;
    assert_eq!(approval_state.try_get::<String, _>("state")?, "approved");
    assert_eq!(
        approval_state.try_get::<String, _>("decided_by")?,
        "admin@example.com"
    );
    assert_eq!(
        approval_state.try_get::<String, _>("rationale")?,
        "approved for this runtime"
    );
    let grant = sqlx::query(
        "SELECT runtime_uid, dimension, granted_value \
         FROM grants WHERE approval_id = $1",
    )
    .bind(parked.approval_id)
    .fetch_one(store.pool())
    .await?;
    assert_eq!(
        grant.try_get::<String, _>("runtime_uid")?,
        "runtime-evidence-a"
    );
    assert_eq!(grant.try_get::<String, _>("dimension")?, "budget");
    assert_eq!(
        grant.try_get::<serde_json::Value, _>("granted_value")?,
        serde_json::to_value(&deltas[0])?
    );
    assert_eq!(
        store
            .grants_for_runtime("runtime-evidence-a", "engineer-evidence", 1)
            .await?,
        deltas,
        "the approved runtime must read back its structured grant"
    );
    assert!(
        store
            .grants_for_runtime("runtime-evidence-a", "analyst-evidence", 1)
            .await?
            .is_empty(),
        "equal revision numbers in different member-role envelope streams must not share authority"
    );
    assert!(
        store
            .grants_for_runtime("runtime-evidence-b", "engineer-evidence", 1)
            .await?
            .is_empty(),
        "a second runtime must never inherit the first runtime's grant"
    );
    let retried = store
        .approve_admission(ApproveAdmission {
            approval_id: parked.approval_id,
            decided_by: "backup-admin@example.org",
            rationale: "recover the previously authorized apply",
            evidence_url: "https://jira.example.com/browse/PROJ-123",
            expires_at: "2999-01-01T00:00:00Z",
        })
        .await
        .map_err(|error| {
            io::Error::other(format!(
                "another admin must be able to retry an authorized apply after a transient failure: {error}"
            ))
        })?;
    assert_eq!(retried, approved);
    let grant_count =
        sqlx::query("SELECT count(*)::bigint AS count FROM grants WHERE approval_id = $1")
            .bind(parked.approval_id)
            .fetch_one(store.pool())
            .await?
            .try_get::<i64, _>("count")?;
    assert_eq!(
        grant_count, 1,
        "an approval retry must not duplicate its grant rows"
    );
    let next_escalation = store
        .park_rejection(ParkRejection {
            runtime_uid: "runtime-evidence-a",
            runtime_namespace: "team-a",
            runtime_name: "runtime-evidence-a",
            spec_digest: "digest-evidence-a",
            base_spec_digest: "base-digest-evidence-a",
            base_pending_approval_digest: None,
            base_spec: &base_spec,
            envelope_revision: 1,
            deltas: &deltas,
            proposed_spec: &proposed_spec,
            actor: "alice@example.com",
            member_role: "engineer-evidence",
        })
        .await?;
    assert_ne!(
        next_escalation.approval_id, parked.approval_id,
        "a completed approval must not permanently capture a later identical escalation"
    );
    assert_eq!(next_escalation.decision_key, None);
    assert_eq!(next_escalation.evidence_url, None);
    let active_application = store
        .grant_application("runtime-evidence-a")
        .await?
        .ok_or_else(|| {
            io::Error::other(
                "an approved grant must remain durable controller work until its spec converges",
            )
        })?;
    let filing_claim = store
        .claim_decision_filing(next_escalation.approval_id)
        .await?;
    let filing_token = filing_claim
        .token
        .ok_or_else(|| io::Error::other("new escalation did not receive a filing lease"))?;
    assert!(
        store
            .retire_pending_approval_if_superseded(
                next_escalation.approval_id,
                active_application.approval_id,
                "runtime-evidence-a",
                "steward-apiserver",
                "superseded by an active approval during create convergence",
            )
            .await?
            .is_some(),
        "an active filing lease must not keep a superseded approval pending"
    );
    assert!(
        !store
            .pending_approvals()
            .await?
            .iter()
            .any(|pending| pending.approval_id == next_escalation.approval_id),
        "a retired loser must not remain reachable through the approval queue"
    );
    store
        .complete_decision_filing(
            next_escalation.approval_id,
            filing_token,
            "PROJ-456",
            "https://jira.example.com/browse/PROJ-456",
        )
        .await?;
    assert_eq!(
        store.approval_for_filing(next_escalation.approval_id).await,
        Err(StoreError::ApprovalNotPending),
        "a retired loser must never be filed or approved later"
    );
    let retired_reference = sqlx::query(
        "SELECT state, decision_key, evidence_url \
         FROM approvals \
         WHERE id = $1",
    )
    .bind(next_escalation.approval_id)
    .fetch_one(store.pool())
    .await?;
    assert_eq!(retired_reference.try_get::<String, _>("state")?, "rejected");
    assert_eq!(
        retired_reference.try_get::<Option<String>, _>("decision_key")?,
        Some("PROJ-456".to_owned()),
        "retirement must preserve the completed external decision reference"
    );
    assert_eq!(
        retired_reference.try_get::<Option<String>, _>("evidence_url")?,
        Some("https://jira.example.com/browse/PROJ-456".to_owned())
    );
    let recoverable_escalation = store
        .park_rejection(ParkRejection {
            runtime_uid: "runtime-evidence-a",
            runtime_namespace: "team-a",
            runtime_name: "runtime-evidence-a",
            spec_digest: "digest-evidence-a",
            base_spec_digest: "base-digest-evidence-a",
            base_pending_approval_digest: None,
            base_spec: &base_spec,
            envelope_revision: 1,
            deltas: &deltas,
            proposed_spec: &proposed_spec,
            actor: "alice@example.com",
            member_role: "engineer-evidence",
        })
        .await?;
    let abandoned_claim = store
        .claim_decision_filing(recoverable_escalation.approval_id)
        .await?;
    let abandoned_token = abandoned_claim
        .token
        .ok_or_else(|| io::Error::other("recoverable escalation did not receive a filing lease"))?;
    assert!(
        store
            .retire_pending_approval_if_superseded(
                recoverable_escalation.approval_id,
                active_application.approval_id,
                "runtime-evidence-a",
                "steward-apiserver",
                "superseded by an active approval during create convergence",
            )
            .await?
            .is_some()
    );
    sqlx::query(
        "UPDATE approvals \
         SET decision_filing_started_at = clock_timestamp() - interval '6 minutes' \
         WHERE id = $1",
    )
    .bind(recoverable_escalation.approval_id)
    .execute(store.pool())
    .await?;
    let recovered_claim = store
        .claim_decision_filing(recoverable_escalation.approval_id)
        .await?;
    let recovered_token = recovered_claim
        .token
        .ok_or_else(|| io::Error::other("expired filing lease was not recovered"))?;
    assert_ne!(
        recovered_token, abandoned_token,
        "recovery must replace the abandoned filing lease"
    );
    store
        .complete_decision_filing(
            recoverable_escalation.approval_id,
            recovered_token,
            "PROJ-789",
            "https://jira.example.com/browse/PROJ-789",
        )
        .await?;
    let recovered_reference = sqlx::query(
        "SELECT state, decision_key, evidence_url \
         FROM approvals \
         WHERE id = $1",
    )
    .bind(recoverable_escalation.approval_id)
    .fetch_one(store.pool())
    .await?;
    assert_eq!(
        recovered_reference.try_get::<String, _>("state")?,
        "rejected"
    );
    assert_eq!(
        recovered_reference.try_get::<Option<String>, _>("decision_key")?,
        Some("PROJ-789".to_owned()),
        "a replacement worker must be able to finish the retired approval's external record"
    );
    let current_escalation = store
        .park_rejection(ParkRejection {
            runtime_uid: "runtime-evidence-a",
            runtime_namespace: "team-a",
            runtime_name: "runtime-evidence-a",
            spec_digest: "digest-evidence-a",
            base_spec_digest: "base-digest-evidence-a",
            base_pending_approval_digest: None,
            base_spec: &base_spec,
            envelope_revision: 1,
            deltas: &deltas,
            proposed_spec: &proposed_spec,
            actor: "alice@example.com",
            member_role: "engineer-evidence",
        })
        .await?;
    assert_eq!(
        store
            .revoke_runtime_grants(
                "runtime-evidence-a",
                "admin@example.com",
                "winner revoked before convergence",
            )
            .await?,
        1
    );
    assert!(
        store
            .retire_pending_approval_if_superseded(
                current_escalation.approval_id,
                active_application.approval_id,
                "runtime-evidence-a",
                "steward-apiserver",
                "superseded by an active approval during create convergence",
            )
            .await?
            .is_none(),
        "a revoked winner must not authorize retirement"
    );
    assert!(
        store
            .pending_approvals()
            .await?
            .iter()
            .any(|pending| pending.approval_id == current_escalation.approval_id),
        "the current escalation must remain pending after winner revocation"
    );
    store
        .insert_envelope(
            "engineer-evidence",
            &envelope("150.00", 2),
            "admin@example.com",
        )
        .await?;
    assert!(
        store
            .grants_for_runtime("runtime-evidence-a", "engineer-evidence", 1)
            .await?
            .is_empty(),
        "authoring a new role envelope must atomically retire older authority"
    );
    assert!(
        store.grant_reversion("runtime-evidence-a").await?.is_some(),
        "superseding an unapplied grant must produce durable reconciliation work"
    );
    Ok(())
}

#[tokio::test]
async fn s4_retirement_checks_expiry_after_waiting_for_authority_locks()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the S4 Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let runtime_uid = format!("runtime-expiry-{suffix}");
    let member_role = format!("engineer-expiry-{suffix}");
    let proposed = proposed_spec();
    let base = base_spec();
    let deltas = budget_deltas();
    store
        .insert_envelope(&member_role, &envelope("200.00", 1), "admin@example.com")
        .await?;
    let winner = store
        .park_rejection(ParkRejection {
            runtime_uid: &runtime_uid,
            runtime_namespace: "team-a",
            runtime_name: "runtime-expiry",
            spec_digest: &format!("winner-digest-{suffix}"),
            base_spec_digest: &format!("winner-base-digest-{suffix}"),
            base_pending_approval_digest: None,
            base_spec: &base,
            envelope_revision: 1,
            deltas: &deltas,
            proposed_spec: &proposed,
            actor: "alice@example.com",
            member_role: &member_role,
        })
        .await?;
    store
        .link_decision_reference(
            winner.approval_id,
            "PROJ-123",
            "https://jira.example.com/browse/PROJ-123",
        )
        .await?;
    let expires_at =
        sqlx::query_scalar::<_, String>("SELECT (clock_timestamp() + interval '1 second')::text")
            .fetch_one(store.pool())
            .await?;
    store
        .approve_admission(ApproveAdmission {
            approval_id: winner.approval_id,
            decided_by: "admin@example.com",
            rationale: "short-lived authority for lock timing",
            evidence_url: "https://jira.example.com/browse/PROJ-123",
            expires_at: &expires_at,
        })
        .await?;
    let loser = store
        .park_rejection(ParkRejection {
            runtime_uid: &runtime_uid,
            runtime_namespace: "team-a",
            runtime_name: "runtime-expiry",
            spec_digest: &format!("loser-digest-{suffix}"),
            base_spec_digest: &format!("loser-base-digest-{suffix}"),
            base_pending_approval_digest: None,
            base_spec: &base,
            envelope_revision: 1,
            deltas: &deltas,
            proposed_spec: &proposed,
            actor: "alice@example.com",
            member_role: &member_role,
        })
        .await?;
    let mut blocker = store.pool().begin().await?;
    sqlx::query("SELECT id FROM approvals WHERE id = $1 FOR UPDATE")
        .bind(winner.approval_id)
        .execute(&mut *blocker)
        .await?;

    let retirement = store.retire_pending_approval_if_superseded(
        loser.approval_id,
        winner.approval_id,
        &runtime_uid,
        "steward-apiserver",
        "superseded by an active approval during create convergence",
    );
    let release = async move {
        sqlx::query("SELECT pg_sleep(2)")
            .execute(&mut *blocker)
            .await?;
        blocker.commit().await
    };
    let (retirement, release) = tokio::join!(retirement, release);
    release?;
    assert!(
        retirement?.is_none(),
        "authority that expires while retirement waits for its locks must not retire the loser"
    );
    assert!(
        store
            .pending_approvals()
            .await?
            .iter()
            .any(|approval| approval.approval_id == loser.approval_id),
        "the escalation must remain pending when the alleged winner has expired"
    );
    Ok(())
}

#[tokio::test]
async fn s4_decision_filing_claim_serializes_concurrent_retries() -> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the S4 Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let runtime_uid = format!("runtime-filing-{suffix}");
    let proposed = proposed_spec();
    let base = base_spec();
    let deltas = budget_deltas();
    let parked = store
        .park_rejection(ParkRejection {
            runtime_uid: &runtime_uid,
            runtime_namespace: "team-a",
            runtime_name: "runtime-filing",
            spec_digest: &format!("digest-{suffix}"),
            base_spec_digest: &format!("base-digest-{suffix}"),
            base_pending_approval_digest: None,
            base_spec: &base,
            envelope_revision: 1,
            deltas: &deltas,
            proposed_spec: &proposed,
            actor: "alice@example.com",
            member_role: "engineer-filing",
        })
        .await?;

    let (left, right) = tokio::join!(
        store.claim_decision_filing(parked.approval_id),
        store.claim_decision_filing(parked.approval_id),
    );
    let (claim, blocked) = match (left, right) {
        (Ok(claim), Err(error)) | (Err(error), Ok(claim)) => (claim, error),
        result => {
            return Err(io::Error::other(format!(
                "exactly one concurrent filing claim must succeed: {result:?}"
            ))
            .into());
        }
    };
    assert_eq!(blocked, StoreError::DecisionFilingInProgress);
    let token = claim
        .token
        .ok_or_else(|| io::Error::other("new filing claim had no lease token"))?;
    store
        .complete_decision_filing(
            parked.approval_id,
            token,
            "PROJ-123",
            "https://jira.example.com/browse/PROJ-123",
        )
        .await?;
    let replay = store.claim_decision_filing(parked.approval_id).await?;
    assert_eq!(replay.token, None);
    assert_eq!(replay.filing.decision_key.as_deref(), Some("PROJ-123"));
    Ok(())
}

#[tokio::test]
async fn s4_pending_create_provenance_survives_every_authority_transition()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_DATABASE_URL is required for the S4 Postgres test")
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool);
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let runtime_uid = format!("runtime-provenance-{suffix}");
    let edit_runtime_uid = format!("runtime-edit-provenance-{suffix}");
    let invalid_runtime_uid = format!("runtime-invalid-provenance-{suffix}");
    let member_role = format!("engineer-provenance-{suffix}");
    let marker_digest = format!("request-digest-{suffix}");
    let proposed = proposed_spec();
    let base = base_spec();
    let deltas = budget_deltas();
    store
        .insert_envelope(&member_role, &envelope("200.00", 1), "admin@example.com")
        .await?;

    let parked = store
        .park_rejection(ParkRejection {
            runtime_uid: &runtime_uid,
            runtime_namespace: "team-a",
            runtime_name: "runtime-provenance",
            spec_digest: &marker_digest,
            base_spec_digest: &format!("base-digest-{suffix}"),
            base_pending_approval_digest: Some(&marker_digest),
            base_spec: &base,
            envelope_revision: 1,
            deltas: &deltas,
            proposed_spec: &proposed,
            actor: "alice@example.com",
            member_role: &member_role,
        })
        .await?;
    let pending = store
        .pending_approvals()
        .await?
        .into_iter()
        .find(|approval| approval.approval_id == parked.approval_id)
        .ok_or_else(|| io::Error::other("parked approval was not queryable"))?;
    assert_eq!(
        pending.base_pending_approval_digest.as_deref(),
        Some(marker_digest.as_str()),
        "parking must retain the exact pending marker provenance"
    );

    store
        .link_decision_reference(
            parked.approval_id,
            "PROJ-123",
            "https://jira.example.com/browse/PROJ-123",
        )
        .await?;
    let candidate = store
        .approval_candidate(
            parked.approval_id,
            "https://jira.example.com/browse/PROJ-123",
        )
        .await?;
    assert_eq!(
        candidate.base_pending_approval_digest.as_deref(),
        Some(marker_digest.as_str())
    );
    assert_eq!(candidate.actor, "alice@example.com");
    store
        .approve_admission(ApproveAdmission {
            approval_id: parked.approval_id,
            decided_by: "admin@example.com",
            rationale: "bounded initial-create authority",
            evidence_url: "https://jira.example.com/browse/PROJ-123",
            expires_at: "2999-01-01T00:00:00Z",
        })
        .await?;

    let application = store
        .grant_application(&runtime_uid)
        .await?
        .ok_or_else(|| io::Error::other("active grant application was not queryable"))?;
    assert_eq!(
        application
            .application
            .base_pending_approval_digest
            .as_deref(),
        Some(marker_digest.as_str()),
        "application must retain initial-create provenance"
    );
    let retired = store
        .retire_pending_approval_if_superseded(
            parked.approval_id,
            parked.approval_id,
            &runtime_uid,
            "steward-controller",
            "validate authority before convergence",
        )
        .await?
        .ok_or_else(|| io::Error::other("active authority did not validate during retirement"))?;
    assert_eq!(
        retired.base_pending_approval_digest.as_deref(),
        Some(marker_digest.as_str()),
        "the locked retirement transition must return the same provenance"
    );

    assert_eq!(
        store
            .revoke_runtime_grants(&runtime_uid, "admin@example.com", "scope ended")
            .await?,
        1
    );
    let reversion = store
        .grant_reversion(&runtime_uid)
        .await?
        .ok_or_else(|| io::Error::other("inactive initial-create grant was not reversible"))?;
    assert_eq!(
        reversion.base_pending_approval_digest.as_deref(),
        Some(marker_digest.as_str()),
        "reversion must restore the exact marker persisted at parking"
    );

    let edit = store
        .park_rejection(ParkRejection {
            runtime_uid: &edit_runtime_uid,
            runtime_namespace: "team-a",
            runtime_name: "runtime-edit-provenance",
            spec_digest: &format!("edit-digest-{suffix}"),
            base_spec_digest: &format!("edit-base-digest-{suffix}"),
            base_pending_approval_digest: None,
            base_spec: &base,
            envelope_revision: 1,
            deltas: &deltas,
            proposed_spec: &proposed,
            actor: "alice@example.com",
            member_role: &member_role,
        })
        .await?;
    let edit_pending = store
        .pending_approvals()
        .await?
        .into_iter()
        .find(|approval| approval.approval_id == edit.approval_id)
        .ok_or_else(|| io::Error::other("edit approval was not queryable"))?;
    assert_eq!(
        edit_pending.base_pending_approval_digest, None,
        "ordinary edit escalation must not acquire initial-create provenance"
    );

    let empty_marker = store
        .park_rejection(ParkRejection {
            runtime_uid: &invalid_runtime_uid,
            runtime_namespace: "team-a",
            runtime_name: "runtime-invalid-provenance",
            spec_digest: &format!("invalid-digest-{suffix}"),
            base_spec_digest: &format!("invalid-base-digest-{suffix}"),
            base_pending_approval_digest: Some(""),
            base_spec: &base,
            envelope_revision: 1,
            deltas: &deltas,
            proposed_spec: &proposed,
            actor: "alice@example.com",
            member_role: &member_role,
        })
        .await;
    assert!(
        empty_marker.is_err(),
        "the migration must reject empty pending-marker provenance"
    );
    Ok(())
}

#[tokio::test]
async fn governed_connection_operations_are_serialized_restart_safe_and_hidden_from_agent_runs()
-> Result<(), Box<dyn Error>> {
    let database_url = env::var("STEWARD_TEST_DATABASE_URL").map_err(|_| {
        io::Error::other(
            "STEWARD_TEST_DATABASE_URL is required for the connection-operation Postgres test",
        )
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&database_url)
        .await?;
    let store = PgStore::new(pool.clone());
    store.migrate().await?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let email = Email::parse(format!("connections-{suffix}@example.com"))?;
    let identity = google_identity(format!("connections-subject-{suffix}"), email.as_str())?;
    let principal = store
        .register_canonical_identity(&identity, "identity-admin")
        .await?;

    let status = reserve_governed_connection(
        &store,
        &principal.user_id,
        &email,
        ConnectionOperationKind::Status,
        true,
        &format!("status-{suffix}"),
    )
    .await?;
    assert!(status.inserted);
    let bridge_runtime_uid = format!("bridge-runtime-{suffix}");
    store
        .bind_task_runtime(
            status.record.task_uid,
            &bridge_runtime_uid,
            TaskPhase::Submitted,
        )
        .await?;
    let by_runtime = store
        .connection_operation_for_runtime(&bridge_runtime_uid)
        .await?
        .ok_or_else(|| {
            io::Error::other("exact bridge runtime UID did not resolve its internal authority")
        })?;
    assert_eq!(by_runtime.operation_id, status.record.operation_id);
    assert_eq!(
        by_runtime.runtime_uid.as_deref(),
        Some(bridge_runtime_uid.as_str())
    );
    assert!(
        store
            .connection_operation_for_runtime(&format!("unrelated-{suffix}"))
            .await?
            .is_none(),
        "an unrelated or long-running runtime UID must not resolve bridge authority"
    );
    let joined_status = reserve_governed_connection(
        &store,
        &principal.user_id,
        &email,
        ConnectionOperationKind::Status,
        true,
        &format!("status-duplicate-{suffix}"),
    )
    .await?;
    assert!(!joined_status.inserted);
    assert_eq!(
        joined_status.record.operation_id,
        status.record.operation_id
    );

    let disconnect = reserve_governed_connection(
        &store,
        &principal.user_id,
        &email,
        ConnectionOperationKind::Disconnect,
        true,
        &format!("disconnect-{suffix}"),
    )
    .await?;
    assert!(
        disconnect.inserted,
        "a mutation must preempt an in-flight status read"
    );
    assert_eq!(
        store
            .connection_operation(status.record.operation_id, &principal.user_id)
            .await?
            .ok_or_else(|| io::Error::other("preempted status operation disappeared"))?
            .operation_state,
        ConnectionOperationState::Failed,
        "mutation precedence must durably stop the in-flight status operation"
    );
    assert_eq!(
        reserve_governed_connection(
            &store,
            &principal.user_id,
            &email,
            ConnectionOperationKind::Status,
            false,
            &format!("status-during-disconnect-{suffix}"),
        )
        .await,
        Err(StoreError::ConnectionOperationConflict),
        "polling must not create a second runtime while a mutation is active"
    );
    store
        .complete_connection_operation(
            disconnect.record.operation_id,
            &serde_json::json!({"disconnected": true}),
            None,
            None,
            connection_operation_retention(),
        )
        .await?;
    assert_eq!(
        store
            .fail_connection_operation(disconnect.record.operation_id, "late_failure")
            .await,
        Err(StoreError::InvalidConnectionOperation),
        "a stale reconciler must never overwrite an atomically persisted success"
    );
    assert_eq!(
        store
            .connection_operation(disconnect.record.operation_id, &principal.user_id)
            .await?
            .ok_or_else(|| io::Error::other("completed disconnect disappeared"))?
            .operation_state,
        ConnectionOperationState::Succeeded
    );
    sqlx::query(
        "UPDATE connection_operations SET updated_at = now() - interval '151 seconds' \
         WHERE operation_id = $1",
    )
    .bind(disconnect.record.operation_id)
    .execute(&pool)
    .await?;
    assert!(
        store
            .mark_stalled_connection_cleanup(disconnect.record.operation_id, 150)
            .await?,
        "teardown beyond the fixed grace period must create a durable finding"
    );
    let stalled = store
        .connection_operation(disconnect.record.operation_id, &principal.user_id)
        .await?
        .ok_or_else(|| io::Error::other("stalled cleanup audit record disappeared"))?;
    assert_eq!(stalled.cleanup_state, "stalled");
    assert_eq!(stalled.cleanup_finding.as_deref(), Some("teardown_stalled"));

    assert!(
        store.agent_run(disconnect.record.task_uid).await?.is_none(),
        "connection operations must not appear in agent-run detail"
    );
    assert!(
        store
            .agent_run_timeline(disconnect.record.task_uid)
            .await?
            .is_none(),
        "connection operations must not appear in agent-run timelines"
    );
    assert!(
        store
            .task_for_submitter(
                disconnect.record.task_uid,
                CONNECTIONS_SERVICE,
                principal.user_id.as_str(),
            )
            .await?
            .is_none(),
        "connection operations must not appear in generic task APIs"
    );

    let start = reserve_governed_connection(
        &store,
        &principal.user_id,
        &email,
        ConnectionOperationKind::Start,
        true,
        &format!("start-{suffix}"),
    )
    .await?;
    let authorization_url =
        format!("https://github.example.test/login/oauth/authorize?state={suffix}");
    store
        .complete_connection_operation(
            start.record.operation_id,
            &serde_json::json!({"authorizationUrl": authorization_url}),
            Some(&authorization_url),
            Some(&format!("sha256:{}", "a".repeat(64))),
            connection_operation_retention(),
        )
        .await?;
    let reused_start = reserve_governed_connection(
        &store,
        &principal.user_id,
        &email,
        ConnectionOperationKind::Start,
        true,
        &format!("start-duplicate-{suffix}"),
    )
    .await?;
    assert!(!reused_start.inserted);
    assert_eq!(reused_start.record.operation_id, start.record.operation_id);
    assert_eq!(
        reused_start.record.oauth_phase,
        ConnectionOAuthPhase::Pending
    );
    assert_eq!(
        reserve_governed_connection(
            &store,
            &principal.user_id,
            &email,
            ConnectionOperationKind::Disconnect,
            true,
            &format!("disconnect-pending-{suffix}"),
        )
        .await,
        Err(StoreError::ConnectionOAuthFlowPending)
    );

    let pending_disconnected_status = reserve_governed_connection(
        &store,
        &principal.user_id,
        &email,
        ConnectionOperationKind::Status,
        false,
        &format!("pending-disconnected-status-{suffix}"),
    )
    .await?;
    assert!(pending_disconnected_status.record.uncached_status);
    store
        .complete_connection_operation(
            pending_disconnected_status.record.operation_id,
            &serde_json::json!({"connected": false}),
            None,
            None,
            connection_operation_retention(),
        )
        .await?;
    let post_callback_status = reserve_governed_connection(
        &store,
        &principal.user_id,
        &email,
        ConnectionOperationKind::Status,
        true,
        &format!("post-callback-status-{suffix}"),
    )
    .await?;
    assert!(
        post_callback_status.inserted,
        "a disconnected cache cannot hide an OAuth callback while its flow remains pending"
    );
    assert_ne!(
        post_callback_status.record.operation_id,
        pending_disconnected_status.record.operation_id
    );
    store
        .complete_connection_operation(
            post_callback_status.record.operation_id,
            &serde_json::json!({
                "connected": true,
                "email": email.as_str(),
                "scopesRequired": [],
                "scopesGranted": [],
                "missingScopes": []
            }),
            None,
            None,
            connection_operation_retention(),
        )
        .await?;
    store
        .complete_pending_connection_oauth_flow(&principal.user_id)
        .await?;
    let post_callback_disconnect = reserve_governed_connection(
        &store,
        &principal.user_id,
        &email,
        ConnectionOperationKind::Disconnect,
        true,
        &format!("disconnect-after-callback-{suffix}"),
    )
    .await?;
    assert!(post_callback_disconnect.inserted);
    let redacted_start = store
        .connection_operation(start.record.operation_id, &principal.user_id)
        .await?
        .ok_or_else(|| io::Error::other("start audit record disappeared"))?;
    assert_eq!(redacted_start.oauth_phase, ConnectionOAuthPhase::Completed);
    assert_eq!(redacted_start.authorization_url, None);
    store
        .complete_connection_operation(
            post_callback_disconnect.record.operation_id,
            &serde_json::json!({"disconnected": true}),
            None,
            None,
            connection_operation_retention(),
        )
        .await?;

    let expiring_start = reserve_governed_connection(
        &store,
        &principal.user_id,
        &email,
        ConnectionOperationKind::Start,
        true,
        &format!("expiring-start-{suffix}"),
    )
    .await?;
    store
        .complete_connection_operation(
            expiring_start.record.operation_id,
            &serde_json::json!({"authorizationUrl": authorization_url}),
            Some(&authorization_url),
            Some(&format!("sha256:{}", "b".repeat(64))),
            connection_operation_retention(),
        )
        .await?;
    let lifetime_seconds: f64 = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM (flow_expires_at - flow_created_at))::float8 \
         FROM connection_operations WHERE operation_id = $1",
    )
    .bind(expiring_start.record.operation_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        lifetime_seconds, 630.0,
        "the 600-second upstream lifetime must retain the 30-second conservative skew buffer"
    );
    sqlx::query(
        "UPDATE connection_operations SET flow_expires_at = now() - interval '1 second' \
         WHERE operation_id = $1",
    )
    .bind(expiring_start.record.operation_id)
    .execute(&pool)
    .await?;
    let replacement_start = reserve_governed_connection(
        &store,
        &principal.user_id,
        &email,
        ConnectionOperationKind::Start,
        true,
        &format!("replacement-start-{suffix}"),
    )
    .await?;
    assert!(replacement_start.inserted);
    assert_ne!(
        replacement_start.record.operation_id,
        expiring_start.record.operation_id
    );
    let expired_start = store
        .connection_operation(expiring_start.record.operation_id, &principal.user_id)
        .await?
        .ok_or_else(|| io::Error::other("expired start audit record disappeared"))?;
    assert_eq!(expired_start.oauth_phase, ConnectionOAuthPhase::Expired);
    assert_eq!(expired_start.authorization_url, None);
    Ok(())
}
