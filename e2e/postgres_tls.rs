use std::borrow::Cow;
use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::Router;
use axum::routing::{get, post};
use sqlx::migrate::Migrator;
use sqlx::postgres::PgPoolOptions;
use sqlx::types::Uuid;
use steward_apiserver::BoxFuture;
use steward_apiserver::connections::{
    ConnectionBrokerError, ConnectionSession, ConnectionStartOperation, ConnectionSubject,
    ProviderConnectionBroker, ProviderConnectionStatus, ReservedConnectionStart,
};
use steward_apiserver::governed_connections::{
    DirectConnectionStatusConfig, DirectConnectionStatusReader, SplitConnectionsBroker,
};
use steward_store::{
    BrowserMemberInvitation, BrowserMemberInvitationOutcome, BrowserMemberStateAction,
    BrowserMemberStateChange, BrowserRbacAssignment, BrowserRbacAssignmentAction,
    BrowserRbacAssignmentChange, BrowserTaskVersionPublication, FederatedSubjectAssociation,
    FederatedSubjectAssociationMethod, FederatedSubjectAuditAction, FederatedSubjectDisable,
    FederatedSubjectObservation, FederatedSubjectState, FederatedSubjectUnlink, PgStore,
    StoreError,
};
use steward_types::direct_package::SourceProvenance;
use steward_types::{
    CanonicalUserId, Email, GOOGLE_ORGANIZATION_ISSUER, OrganizationId, OrganizationIdentityPolicy,
};
use tokio::net::TcpListener;
use tokio::sync::Barrier;
use tokio::task::JoinHandle;

struct ServerGuard(JoinHandle<Result<(), io::Error>>);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct TemporaryFile(PathBuf);

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[derive(Clone)]
struct NoopConnectionMutations;

impl ProviderConnectionBroker<String> for NoopConnectionMutations {
    fn status<'a>(
        &'a self,
        _session: &'a ConnectionSession<String>,
    ) -> BoxFuture<'a, Result<ProviderConnectionStatus, ConnectionBrokerError>> {
        Box::pin(async { Err(ConnectionBrokerError::Unavailable) })
    }

    fn start<'a>(
        &'a self,
        _session: &'a ConnectionSession<String>,
    ) -> BoxFuture<'a, Result<ReservedConnectionStart, ConnectionBrokerError>> {
        Box::pin(async { Err(ConnectionBrokerError::Unavailable) })
    }

    fn start_operation<'a>(
        &'a self,
        _session: &'a ConnectionSession<String>,
        _operation_id: Uuid,
    ) -> BoxFuture<'a, Result<Option<ConnectionStartOperation>, ConnectionBrokerError>> {
        Box::pin(async { Err(ConnectionBrokerError::Unavailable) })
    }

    fn disconnect<'a>(
        &'a self,
        _session: &'a ConnectionSession<String>,
    ) -> BoxFuture<'a, Result<ReservedConnectionStart, ConnectionBrokerError>> {
        Box::pin(async { Err(ConnectionBrokerError::Unavailable) })
    }
}

fn migration_set(maximum_version: Option<i64>) -> Migrator {
    let embedded = sqlx::migrate!("../migrations");
    Migrator {
        migrations: Cow::Owned(
            embedded
                .migrations
                .iter()
                .filter(|migration| {
                    maximum_version.is_none_or(|maximum| migration.version <= maximum)
                })
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: false,
        no_tx: false,
    }
}

#[tokio::test]
async fn tls_required_postgres_accepts_store_migrations() -> Result<(), Box<dyn Error>> {
    let plaintext_url = env::var("STEWARD_TEST_PLAINTEXT_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_PLAINTEXT_DATABASE_URL is required for the TLS test")
    })?;
    let plaintext = PgPoolOptions::new()
        .max_connections(1)
        .connect(&plaintext_url)
        .await;
    assert!(
        plaintext.is_err(),
        "the PostgreSQL fixture must reject plaintext connections"
    );

    let tls_url = env::var("STEWARD_TEST_TLS_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_TLS_DATABASE_URL is required for the TLS test")
    })?;
    let verified_tls_url = env::var("STEWARD_TEST_VERIFIED_TLS_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_VERIFIED_TLS_DATABASE_URL is required for the TLS test")
    })?;
    let encrypted_store = PgStore::connect(&tls_url).await.map_err(|error| {
        io::Error::other(format!(
            "Steward must connect when PostgreSQL requires encryption: {error}"
        ))
    })?;
    let encrypted =
        sqlx::query_scalar::<_, bool>("SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()")
            .fetch_one(encrypted_store.pool())
            .await?;
    assert!(
        encrypted,
        "sslmode=require must establish an encrypted session"
    );
    let wrong_ca_url = env::var("STEWARD_TEST_WRONG_CA_DATABASE_URL").map_err(|_| {
        io::Error::other("STEWARD_TEST_WRONG_CA_DATABASE_URL is required for the TLS test")
    })?;
    assert!(
        PgStore::connect(&wrong_ca_url).await.is_err(),
        "Steward must fail closed when PostgreSQL presents a certificate from another CA"
    );
    let wrong_hostname_url =
        env::var("STEWARD_TEST_WRONG_HOSTNAME_DATABASE_URL").map_err(|_| {
            io::Error::other(
                "STEWARD_TEST_WRONG_HOSTNAME_DATABASE_URL is required for the TLS test",
            )
        })?;
    assert!(
        PgStore::connect(&wrong_hostname_url).await.is_err(),
        "Steward must fail closed when the PostgreSQL certificate does not cover the hostname"
    );
    let store = PgStore::connect(&verified_tls_url).await.map_err(|error| {
        io::Error::other(format!(
            "Steward must connect when PostgreSQL requires sslmode=verify-full: {error}"
        ))
    })?;
    migration_set(Some(38))
        .run(store.pool())
        .await
        .map_err(|error| {
            io::Error::other(format!(
                "Steward v0.1.23 migrations must complete over the required TLS session: {error}"
            ))
        })?;

    seed_v0123_upgrade_fixture(&store).await?;
    let refused = migration_set(None).run(store.pool()).await;
    assert!(
        refused.is_err_and(|error| error.to_string().contains(
            "v0.2 upgrade refuses unfinished Tasks without one complete immutable authority"
        )),
        "v0.2 must fail closed while an unfinished legacy Task has ambiguous authority"
    );
    sqlx::query(
        "UPDATE task_submissions \
         SET phase = 'cancelled', finalize_requested = true, finalized = true, \
             failure_reason = 'upgrade_precondition' \
         WHERE idempotency_key = 'legacy-unfinished'",
    )
    .execute(store.pool())
    .await?;
    migration_set(Some(39))
        .run(store.pool())
        .await
        .map_err(|error| {
            io::Error::other(format!(
                "Steward v0.2 migrations must complete over the required TLS session: {error}"
            ))
        })?;

    assert_v02_upgrade_result(&store).await?;
    let historical_before_federated_identity = historical_task_identity_snapshot(&store).await?;
    migration_set(Some(52))
        .run(store.pool())
        .await
        .map_err(|error| {
            io::Error::other(format!(
                "Steward pre-0053 migrations must complete over the required TLS session: {error}"
            ))
        })?;
    assert_eq!(
        historical_task_identity_snapshot(&store).await?,
        historical_before_federated_identity,
        "migration 0040 must not update, backfill, or reinterpret historical Tasks, runs, or canonical identities"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*)::bigint FROM federated_subjects")
            .fetch_one(store.pool())
            .await?,
        0,
        "migration 0040 must not synthesize federated subjects from historical identity data"
    );
    let maximum_direct_source_provenance =
        seed_maximum_source_provenance_upgrade_fixture(&store).await?;
    seed_finalized_task_provenance_upgrade_fixtures(&store).await?;
    seed_connection_association_upgrade_fixture(&store).await?;
    seed_browser_member_details_upgrade_fixture(&store).await?;
    migration_set(Some(62))
        .run(store.pool())
        .await
        .map_err(|error| {
            io::Error::other(format!(
                "Steward pre-0063 migrations must complete over the required TLS session: {error}"
            ))
        })?;
    seed_connection_failure_detail_upgrade_fixture(&store).await?;
    migration_set(Some(66))
        .run(store.pool())
        .await
        .map_err(|error| {
            io::Error::other(format!(
                "Steward pre-0067 migrations must complete over the required TLS session: {error}"
            ))
        })?;
    let historical_before_task_library = historical_task_identity_snapshot(&store).await?;
    migration_set(None).run(store.pool()).await.map_err(|error| {
        io::Error::other(format!(
            "Steward Task-library migration must complete over the required TLS session: {error}"
        ))
    })?;
    assert_eq!(
        historical_task_identity_snapshot(&store).await?,
        historical_before_task_library,
        "migration 0067 must not rewrite historical Tasks, runs, or canonical identities"
    );
    verify_browser_task_library_upgrade(&store).await?;
    assert_connection_failure_detail_upgrade_result(&store).await?;
    assert_github_repository_automation_upgrade_result(&store).await?;
    assert_maximum_source_provenance_upgrade_result(&store, &maximum_direct_source_provenance)
        .await?;
    assert_finalized_task_provenance_upgrade_result(&store).await?;
    verify_source_provenance_byte_limits(&store).await?;
    assert_connection_association_upgrade_result(&store).await?;
    assert_template_catalog_upgrade_result(&store).await?;
    assert_browser_member_details_upgrade_result(&store).await?;
    verify_federated_subject_lifecycle(&store).await?;
    verify_direct_connection_auto_association(&store).await?;
    verify_pending_member_identity_state(&store).await?;
    verify_browser_member_invitation_lifecycle(&store).await?;

    let tls_active =
        sqlx::query_scalar::<_, bool>("SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()")
            .fetch_one(store.pool())
            .await?;
    assert!(
        tls_active,
        "the successful Steward database session must be encrypted"
    );

    let applied_migrations =
        sqlx::query_scalar::<_, i64>("SELECT count(*)::bigint FROM _sqlx_migrations")
            .fetch_one(store.pool())
            .await?;
    assert!(
        applied_migrations > 0,
        "the TLS database must contain applied Steward migrations"
    );

    let mut transaction = store.pool().begin().await?;
    sqlx::query(
        "CREATE TEMP TABLE connection_runtime_class_probe \
         (LIKE connection_operations INCLUDING CONSTRAINTS) ON COMMIT DROP",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "DO $probe$ \
         DECLARE column_name text; \
         BEGIN \
           FOR column_name IN \
             SELECT attribute.attname \
             FROM pg_attribute attribute \
             WHERE attribute.attrelid = 'pg_temp.connection_runtime_class_probe'::regclass \
               AND attribute.attnum > 0 \
               AND NOT attribute.attisdropped \
               AND attribute.attname <> 'runtime_class' \
           LOOP \
             EXECUTE format( \
               'ALTER TABLE pg_temp.connection_runtime_class_probe DROP COLUMN %I CASCADE', \
               column_name \
             ); \
           END LOOP; \
         END \
         $probe$",
    )
    .execute(&mut *transaction)
    .await?;
    sqlx::query("INSERT INTO connection_runtime_class_probe (runtime_class) VALUES ('')")
        .execute(&mut *transaction)
        .await?;
    let whitespace_runtime_class =
        sqlx::query("INSERT INTO connection_runtime_class_probe (runtime_class) VALUES ('   ')")
            .execute(&mut *transaction)
            .await;
    assert!(
        whitespace_runtime_class.is_err(),
        "the connection runtime class must be empty for the cluster default or non-blank"
    );
    transaction.rollback().await?;

    let authority_kind_exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (\
             SELECT 1 FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = 'task_submissions' \
               AND column_name = 'authority_kind'\
         )",
    )
    .fetch_one(store.pool())
    .await?;
    assert!(
        authority_kind_exists,
        "v0.2 Task rows must identify their durable user or internal authority kind"
    );

    let user_snapshot_exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (\
             SELECT 1 FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = 'task_submissions' \
               AND column_name = 'user_envelope_snapshot'\
         )",
    )
    .fetch_one(store.pool())
    .await?;
    assert!(
        user_snapshot_exists,
        "v0.2 user Tasks must persist the exact approved User Envelope snapshot"
    );

    let new_writer_constraint = sqlx::query_scalar::<_, String>(
        "SELECT pg_get_constraintdef(oid) \
         FROM pg_constraint \
         WHERE conrelid = 'task_submissions'::regclass \
           AND conname = 'task_submissions_new_orchestration_version_required'",
    )
    .fetch_one(store.pool())
    .await?;
    assert!(
        new_writer_constraint.contains("orchestration_version = 3"),
        "new Task writers must use the User-Envelope-only orchestration contract: {new_writer_constraint}"
    );

    Ok(())
}

async fn verify_browser_task_library_upgrade(store: &PgStore) -> Result<(), Box<dyn Error>> {
    let task_id = Uuid::parse_str("00000000-0000-0000-0000-000000000279")?;
    let owner_user_id = "usr_0123456789abcdef0123456789abcdef";
    let shared_roles = vec!["Platform.Eng".to_owned(), "org:eng".to_owned()];
    let v1_digest = format!("steward:sha256:{}", "1".repeat(64));
    let v2_digest = format!("steward:sha256:{}", "2".repeat(64));
    let v1_files = BTreeMap::from([(
        "task-definition.json".to_owned(),
        serde_json::json!({
            "schemaVersion": "steward.task-definition/v2",
            "name": "upgrade-task",
            "version": 1,
            "runtime": { "agentRef": "codex@0.140.0" },
            "promptText": "Write the requested output.",
            "outputs": [{ "path": "out", "kind": "directory", "required": true }]
        })
        .to_string(),
    )]);
    let mut v2_files = v1_files.clone();
    v2_files.insert(
        "task-definition.json".to_owned(),
        serde_json::json!({
            "schemaVersion": "steward.task-definition/v2",
            "name": "upgrade-task",
            "version": 2,
            "runtime": { "agentRef": "codex@0.140.0" },
            "promptText": "Write the revised requested output.",
            "outputs": [{ "path": "out", "kind": "directory", "required": true }]
        })
        .to_string(),
    );

    let v1 = store
        .save_browser_task_version(BrowserTaskVersionPublication {
            task_id,
            owner_user_id,
            name: "upgrade-task",
            shared_roles: &shared_roles,
            version: 1,
            content_digest: &v1_digest,
            package_path: "task-definition.json",
            files: &v1_files,
        })
        .await?;
    let v2 = store
        .save_browser_task_version(BrowserTaskVersionPublication {
            task_id,
            owner_user_id,
            name: "upgrade-task",
            shared_roles: &shared_roles,
            version: 2,
            content_digest: &v2_digest,
            package_path: "task-definition.json",
            files: &v2_files,
        })
        .await?;
    assert_eq!(v1.version, 1);
    assert_eq!(v2.version, 2);
    assert_ne!(v1.content_digest, v2.content_digest);

    let duplicate_digest = format!("steward:sha256:{}", "3".repeat(64));
    let duplicate_name = store
        .save_browser_task_version(BrowserTaskVersionPublication {
            task_id: Uuid::parse_str("00000000-0000-0000-0000-000000000280")?,
            owner_user_id,
            name: "upgrade-task",
            shared_roles: &shared_roles,
            version: 1,
            content_digest: &duplicate_digest,
            package_path: "task-definition.json",
            files: &v1_files,
        })
        .await;
    assert!(
        matches!(duplicate_name, Err(StoreError::TaskIdempotencyConflict)),
        "an owner-scoped duplicate Task name must return the documented conflict"
    );

    let versions = store
        .browser_task_draft_versions(owner_user_id, task_id)
        .await?;
    assert_eq!(
        versions
            .iter()
            .map(|version| version.version)
            .collect::<Vec<_>>(),
        [2, 1],
        "the additive Task library must preserve every immutable saved version"
    );
    assert!(
        store
            .browser_task_version_by_digest(
                "usr_abcdef0123456789abcdef0123456789",
                &shared_roles,
                &v1_digest,
            )
            .await?
            .is_some(),
        "a shared role must resolve an exact historical Task version by digest"
    );
    assert!(
        store
            .browser_task_version_by_digest(
                "usr_abcdef0123456789abcdef0123456789",
                &["analyst".to_owned()],
                &v1_digest,
            )
            .await?
            .is_none(),
        "an unrelated role must not discover a shared Task version"
    );

    let mutation = sqlx::query(
        "UPDATE browser_task_versions SET files = '{}'::jsonb \
         WHERE task_id = $1 AND version = 1",
    )
    .bind(task_id)
    .execute(store.pool())
    .await;
    assert!(
        mutation.is_err(),
        "saved Task package bytes must remain immutable after migration"
    );
    Ok(())
}

async fn verify_pending_member_identity_state(store: &PgStore) -> Result<(), Box<dyn Error>> {
    let organization_id = OrganizationId::parse("org_example")?;
    let email = Email::parse("pending.member@example.com")?;
    let pending = store
        .preprovision_canonical_user(
            &organization_id,
            &email,
            "usr_0123456789abcdef0123456789abcdef",
        )
        .await?;
    assert_eq!(pending.state, "pending");
    assert_eq!(
        pending.invited_by.as_deref(),
        Some("alice@example.com"),
        "member detail must resolve the invitation actor to the current administrator email"
    );
    assert!(pending.display_name.is_none());
    assert!(pending.last_sign_in_at.is_none());
    let assignment = BrowserRbacAssignment::MemberRole("engineer".to_owned());
    store
        .append_browser_rbac_assignment(BrowserRbacAssignmentChange {
            user_id: &pending.user_id,
            assignment: &assignment,
            action: BrowserRbacAssignmentAction::Grant,
            actor: "usr_0123456789abcdef0123456789abcdef",
        })
        .await?;
    let identity = OrganizationIdentityPolicy::new(
        GOOGLE_ORGANIZATION_ISSUER,
        "example.com",
        organization_id.clone(),
    )?
    .validate(
        GOOGLE_ORGANIZATION_ISSUER,
        "pending-member-subject",
        "example.com",
        email.as_str(),
        true,
    )?;
    let activated = store
        .register_canonical_identity(&identity, "browser-oidc")
        .await?;
    store
        .record_browser_sign_in(&activated.user_id, Some("Pending Member"))
        .await?;
    assert_eq!(activated.user_id, pending.user_id);
    assert_eq!(
        store
            .browser_rbac_assignments(&activated.user_id)
            .await?
            .member_roles,
        ["engineer"],
        "the verified sign-in must activate the exact pending member without losing its audited role"
    );
    let active_member = store
        .canonical_user(&activated.user_id)
        .await?
        .expect("activated user");
    assert_eq!(active_member.state, "active");
    assert_eq!(
        active_member.display_name.as_deref(),
        Some("Pending Member")
    );
    assert!(active_member.last_sign_in_at.is_some());
    assert_eq!(
        active_member.invited_by.as_deref(),
        Some("alice@example.com"),
        "activation must not erase who invited the member"
    );
    let administrator = BrowserRbacAssignment::Administrator;
    store
        .append_browser_rbac_assignment(BrowserRbacAssignmentChange {
            user_id: &activated.user_id,
            assignment: &administrator,
            action: BrowserRbacAssignmentAction::Grant,
            actor: "usr_0123456789abcdef0123456789abcdef",
        })
        .await?;
    assert!(matches!(
        store
            .append_browser_rbac_assignment(BrowserRbacAssignmentChange {
                user_id: &activated.user_id,
                assignment: &administrator,
                action: BrowserRbacAssignmentAction::Revoke,
                actor: "usr_0123456789abcdef0123456789abcdef",
            })
            .await,
        Err(StoreError::LastBrowserAdministrator)
    ));
    Ok(())
}

async fn verify_browser_member_invitation_lifecycle(store: &PgStore) -> Result<(), Box<dyn Error>> {
    let organization_id = OrganizationId::parse("org_team-a")?;
    let email = Email::parse("alice@example.org")?;
    let roles = vec!["engineer".to_owned()];
    let outcome = store
        .invite_browser_member(BrowserMemberInvitation {
            organization_id: &organization_id,
            email: &email,
            actor: "usr_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            administrator: true,
            member_roles: &roles,
        })
        .await?;
    let pending = match outcome {
        BrowserMemberInvitationOutcome::Invited(user) => user,
        BrowserMemberInvitationOutcome::AlreadyMember(_) => {
            return Err(io::Error::other("fresh member was not invited").into());
        }
    };
    let assignments = store.browser_rbac_assignments(&pending.user_id).await?;
    assert!(assignments.is_admin);
    assert_eq!(assignments.member_roles, roles);
    assert!(matches!(
        store
            .invite_browser_member(BrowserMemberInvitation {
                organization_id: &organization_id,
                email: &email,
                actor: "usr_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                administrator: false,
                member_roles: &[],
            })
            .await?,
        BrowserMemberInvitationOutcome::AlreadyMember(_)
    ));

    let identity = OrganizationIdentityPolicy::new(
        GOOGLE_ORGANIZATION_ISSUER,
        "example.org",
        organization_id.clone(),
    )?
    .validate(
        GOOGLE_ORGANIZATION_ISSUER,
        "member-lifecycle-subject",
        "example.org",
        email.as_str(),
        true,
    )?;
    let active = store
        .register_canonical_identity(&identity, "browser-oidc")
        .await?;
    assert_eq!(active.user_id, pending.user_id);
    assert!(matches!(
        store
            .change_browser_member_state(BrowserMemberStateChange {
                user_id: &active.user_id,
                action: BrowserMemberStateAction::Disable,
                actor: &active.user_id,
            })
            .await,
        Err(StoreError::SelfBrowserMemberMutation)
    ));
    let other_actor = CanonicalUserId::parse("usr_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")?;
    assert!(matches!(
        store
            .change_browser_member_state(BrowserMemberStateChange {
                user_id: &active.user_id,
                action: BrowserMemberStateAction::Disable,
                actor: &other_actor,
            })
            .await,
        Err(StoreError::LastBrowserAdministrator)
    ));

    let invite_email = Email::parse("bob@example.org")?;
    let first_invitation = match store
        .invite_browser_member(BrowserMemberInvitation {
            organization_id: &organization_id,
            email: &invite_email,
            actor: active.user_id.as_str(),
            administrator: false,
            member_roles: &[],
        })
        .await?
    {
        BrowserMemberInvitationOutcome::Invited(user) => user,
        BrowserMemberInvitationOutcome::AlreadyMember(_) => {
            return Err(io::Error::other("fresh pending invitation already existed").into());
        }
    };
    let revoked = store
        .change_browser_member_state(BrowserMemberStateChange {
            user_id: &first_invitation.user_id,
            action: BrowserMemberStateAction::RevokeInvitation,
            actor: &active.user_id,
        })
        .await?;
    assert_eq!(revoked.state, "revoked");
    let reinvited = match store
        .invite_browser_member(BrowserMemberInvitation {
            organization_id: &organization_id,
            email: &invite_email,
            actor: active.user_id.as_str(),
            administrator: false,
            member_roles: &[],
        })
        .await?
    {
        BrowserMemberInvitationOutcome::Invited(user) => user,
        BrowserMemberInvitationOutcome::AlreadyMember(_) => {
            return Err(io::Error::other("revoked invitation blocked reinvitation").into());
        }
    };
    assert_ne!(reinvited.user_id, first_invitation.user_id);

    let second_identity = OrganizationIdentityPolicy::new(
        GOOGLE_ORGANIZATION_ISSUER,
        "example.org",
        organization_id,
    )?
    .validate(
        GOOGLE_ORGANIZATION_ISSUER,
        "reinvited-member-subject",
        "example.org",
        invite_email.as_str(),
        true,
    )?;
    let second_active = store
        .register_canonical_identity(&second_identity, "browser-oidc")
        .await?;
    let disabled = store
        .change_browser_member_state(BrowserMemberStateChange {
            user_id: &second_active.user_id,
            action: BrowserMemberStateAction::Disable,
            actor: &active.user_id,
        })
        .await?;
    assert_eq!(disabled.state, "disabled");
    let enabled = store
        .change_browser_member_state(BrowserMemberStateChange {
            user_id: &second_active.user_id,
            action: BrowserMemberStateAction::Enable,
            actor: &active.user_id,
        })
        .await?;
    assert_eq!(enabled.state, "active");
    Ok(())
}

async fn seed_browser_member_details_upgrade_fixture(
    store: &PgStore,
) -> Result<(), Box<dyn Error>> {
    sqlx::query(
        "INSERT INTO canonical_users (user_id, organization_id, display_email) \
         VALUES ('usr_33333333333333333333333333333333', 'org_example', \
                 'historical.member@example.com')",
    )
    .execute(store.pool())
    .await?;
    Ok(())
}

async fn assert_browser_member_details_upgrade_result(
    store: &PgStore,
) -> Result<(), Box<dyn Error>> {
    let user_id = CanonicalUserId::parse("usr_33333333333333333333333333333333")?;
    let historical = store.canonical_user(&user_id).await?.ok_or_else(|| {
        io::Error::other("historical browser member disappeared during migration")
    })?;
    assert!(historical.display_name.is_none());
    assert!(historical.last_sign_in_at.is_none());
    assert!(historical.invited_by.is_none());
    assert!(
        !historical.created_at.is_empty(),
        "historical members must receive the existing creation timestamp without rewriting it"
    );

    store
        .record_browser_sign_in(&user_id, Some("Historical Member"))
        .await?;
    let signed_in = store
        .canonical_user(&user_id)
        .await?
        .ok_or_else(|| io::Error::other("signed-in browser member disappeared"))?;
    assert_eq!(signed_in.display_name.as_deref(), Some("Historical Member"));
    assert!(signed_in.last_sign_in_at.is_some());
    Ok(())
}

async fn seed_maximum_source_provenance_upgrade_fixture(
    store: &PgStore,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let task_uid = "00000000-0000-0000-0000-000000000056";
    let operation_id = "00000000-0000-0000-0000-000000001056";
    let mut evidence: serde_json::Value = serde_json::from_str(include_str!(
        "../docs/contracts/task/v2/fixtures/positive/task-binding-evidence.json"
    ))?;
    let maximum_provenance = serde_json::json!({
        "contractVersion": "steward.source-provenance/v1",
        "provider": "github",
        "repository": {
            "id": "123456",
            "ownerId": "7890",
            "name": "n".repeat(512),
        },
        "triggeredSha": format!("git:sha1:{}", "a".repeat(40)),
        "run": {
            "id": "900001",
            "attempt": 1,
        },
        "event": "e".repeat(512),
        "ref": "r".repeat(2048),
        "actorId": "24680",
        "actor": "a".repeat(512),
        "callerWorkflow": {
            "ref": "c".repeat(2048),
            "sha": format!("git:sha1:{}", "b".repeat(40)),
        },
        "reusableWorkflow": {
            "ref": "w".repeat(2048),
            "sha": format!("git:sha1:{}", "c".repeat(40)),
        },
    });
    let frozen_provenance: SourceProvenance = serde_json::from_value(maximum_provenance.clone())?;
    frozen_provenance.validate().map_err(io::Error::other)?;

    evidence["taskUid"] = serde_json::json!(task_uid);
    evidence["sourceProvenance"] = maximum_provenance.clone();
    evidence["envelope"]["revision"] = serde_json::json!(7);
    evidence["envelope"]["digest"] =
        serde_json::json!(format!("steward:sha256:{}", "b".repeat(64)));

    let mut transaction = store.pool().begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO task_submissions (\
             task_uid, idempotency_key, submitter_service, acting_user, acting_user_id, \
             owner, owner_user_id, identity_binding_state, workflow, workflow_name, workflow_version, \
             workflow_digest, user_envelope_instance_id, user_envelope_revision, \
             user_envelope_digest, authority_kind, user_envelope_snapshot, coding_agent_runtime, \
             runtime_uid, runtime_namespace, runtime_name, runtime_ownership, phase, runtime_spec, \
             finalize_requested, finalized, agent_command, execution_binding, direct_task_evidence, envelope_revision, \
             orchestration_version, orchestration_operation_id, candidate_digest, \
             service_envelope_digest, original_admission_decision, original_admission_deltas) \
         SELECT $1::text::uuid, 'migration-0056-max-provenance', submitter_service, acting_user, \
                acting_user_id, owner, owner_user_id, identity_binding_state, workflow, NULL, NULL, \
                NULL, user_envelope_instance_id, user_envelope_revision, user_envelope_digest, \
                authority_kind, user_envelope_snapshot, coding_agent_runtime, NULL, \
                runtime_namespace, 'task-migration-0056', runtime_ownership, 'succeeded', \
                runtime_spec, true, true, agent_command, execution_binding, $3, envelope_revision, \
                orchestration_version, $2::text::uuid, candidate_digest, service_envelope_digest, \
                original_admission_decision, original_admission_deltas \
         FROM task_submissions \
         WHERE task_uid = '00000000-0000-0000-0000-000000000002'",
    )
    .bind(task_uid)
    .bind(operation_id)
    .bind(&evidence)
    .execute(&mut *transaction)
    .await?
    .rows_affected();
    assert_eq!(inserted, 1, "the pre-0056 direct Task fixture must exist");
    sqlx::query(
        "INSERT INTO task_runtime_operations (\
             task_uid, operation_id, state, generation, runtime_ownership, runtime_namespace, \
             runtime_name, inert_manifest_digest, active_manifest_digest) \
         VALUES ($1::text::uuid, $2::text::uuid, 'intent_recorded', 1, 'provisioned', \
                 'steward-workflows', 'task-migration-0056', $3, $4)",
    )
    .bind(task_uid)
    .bind(operation_id)
    .bind(format!("sha256:{}", "1".repeat(64)))
    .bind(format!("sha256:{}", "2".repeat(64)))
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(maximum_provenance)
}

async fn assert_maximum_source_provenance_upgrade_result(
    store: &PgStore,
    expected: &serde_json::Value,
) -> Result<(), Box<dyn Error>> {
    let migrated = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT source_provenance FROM task_submissions \
         WHERE task_uid = '00000000-0000-0000-0000-000000000056'",
    )
    .fetch_one(store.pool())
    .await?;
    assert_eq!(
        migrated, *expected,
        "migration 0056 must backfill every source-provenance value accepted by the frozen contract"
    );
    Ok(())
}

async fn seed_finalized_task_provenance_upgrade_fixtures(
    store: &PgStore,
) -> Result<(), Box<dyn Error>> {
    sqlx::query(
        "UPDATE task_submissions \
         SET phase = 'failed', finalize_requested = true, finalized = true, \
             failure_reason = 'upgrade_fixture' \
         WHERE idempotency_key = 'user-unfinished'",
    )
    .execute(store.pool())
    .await?;

    let task_uid = "00000000-0000-0000-0000-000000000057";
    let operation_id = "00000000-0000-0000-0000-000000001057";
    let authority_digest =
        "sha256:9a572bcefa75b6f2b5b4931d8604c1ad3f3e7560e0e0c2843646ec4f7853ef02";
    let mut transaction = store.pool().begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO task_submissions (\
             task_uid, idempotency_key, submitter_service, acting_user, acting_user_id, \
             owner, owner_user_id, identity_binding_state, workflow, workflow_name, workflow_version, \
             workflow_digest, user_envelope_instance_id, user_envelope_revision, \
             user_envelope_digest, authority_kind, user_envelope_snapshot, internal_authority_id, \
             internal_authority_version, internal_authority_digest, coding_agent_runtime, runtime_uid, \
             runtime_namespace, runtime_name, runtime_ownership, phase, runtime_spec, \
             finalize_requested, finalized, failure_reason, agent_command, execution_binding, \
             direct_task_evidence, envelope_revision, orchestration_version, orchestration_operation_id, \
             candidate_digest, service_envelope_digest, original_admission_decision, original_admission_deltas) \
         SELECT $1::text::uuid, 'migration-0057-finalized-connection', 'steward-connections', \
                acting_user, acting_user_id, owner, owner_user_id, identity_binding_state, \
                'connections.github.status', NULL, NULL, NULL, NULL, NULL, NULL, 'internal', NULL, \
                'steward-connections', 2, $3, coding_agent_runtime, NULL, runtime_namespace, \
                'task-migration-0057', runtime_ownership, 'failed', runtime_spec, true, true, \
                'upgrade_fixture', agent_command, execution_binding, NULL, envelope_revision, \
                orchestration_version, $2::text::uuid, candidate_digest, service_envelope_digest, \
                original_admission_decision, original_admission_deltas \
         FROM task_submissions \
         WHERE task_uid = '00000000-0000-0000-0000-000000000003'",
    )
    .bind(task_uid)
    .bind(operation_id)
    .bind(authority_digest)
    .execute(&mut *transaction)
    .await?
    .rows_affected();
    assert_eq!(inserted, 1, "the pre-0057 internal Task fixture must exist");

    sqlx::query(
        "INSERT INTO task_runtime_operations (\
             task_uid, operation_id, state, generation, runtime_ownership, runtime_namespace, \
             runtime_name, inert_manifest_digest, active_manifest_digest) \
         VALUES ($1::text::uuid, $2::text::uuid, 'intent_recorded', 1, 'provisioned', \
                 'steward-workflows', 'task-migration-0057', $3, $4)",
    )
    .bind(task_uid)
    .bind(operation_id)
    .bind(format!("sha256:{}", "3".repeat(64)))
    .bind(format!("sha256:{}", "4".repeat(64)))
    .execute(&mut *transaction)
    .await?;

    sqlx::query(
        "INSERT INTO connection_operations (\
             operation_id, task_uid, canonical_user_id, provider, operation_kind, \
             submitter_service, authority_id, authority_version, authority_digest, \
             runtime_spec_snapshot, command_snapshot, artifact_trust_mode, bridge_image_digest, \
             mcp_gw_origin, mcp_gw_version, runtime_namespace, runtime_class, \
             idempotency_identity, response_deadline_at) \
         VALUES (\
             $2::text::uuid, $1::text::uuid, 'usr_0123456789abcdef0123456789abcdef', \
             'github', 'status', 'steward-connections', 'steward-connections', 2, $3, \
             '{}'::jsonb, '[]'::jsonb, 'github-attestation', \
             'ghcr.io/example-org/connections-bridge@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', \
             'https://gateway.example.test', '0.4.9', 'steward-workflows', '', \
             'migration-0057-finalized-connection', now() + interval '1 minute'\
         )",
    )
    .bind(task_uid)
    .bind(operation_id)
    .bind(authority_digest)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    Ok(())
}

async fn assert_finalized_task_provenance_upgrade_result(
    store: &PgStore,
) -> Result<(), Box<dyn Error>> {
    let versioned = sqlx::query_as::<_, (Option<serde_json::Value>, String)>(
        "SELECT source_provenance, task_origin FROM task_submissions \
         WHERE idempotency_key = 'user-unfinished'",
    )
    .fetch_one(store.pool())
    .await?;
    assert_eq!(
        versioned,
        (None, "unknown".to_owned()),
        "migration provenance backfills must preserve finalized versioned Workflow history"
    );

    let connection_origin = sqlx::query_scalar::<_, String>(
        "SELECT task_origin FROM task_submissions \
         WHERE idempotency_key = 'migration-0057-finalized-connection'",
    )
    .fetch_one(store.pool())
    .await?;
    assert_eq!(
        connection_origin, "connections",
        "migration 0057 must classify finalized governed connection Tasks"
    );

    let monotonic_trigger_enabled = sqlx::query_scalar::<_, bool>(
        "SELECT tgenabled = 'O' FROM pg_trigger \
         WHERE tgrelid = 'task_submissions'::regclass \
           AND tgname = 'task_commands_are_monotonic'",
    )
    .fetch_one(store.pool())
    .await?;
    assert!(
        monotonic_trigger_enabled,
        "migration backfills must re-enable the finalized-Task monotonicity trigger"
    );

    let rejected = sqlx::query(
        "UPDATE task_submissions SET updated_at = now() \
         WHERE idempotency_key = 'migration-0056-max-provenance'",
    )
    .execute(store.pool())
    .await;
    assert!(
        rejected.is_err_and(|error| error
            .to_string()
            .contains("durable Task commands and terminal observations are monotonic")),
        "the re-enabled monotonicity trigger must still reject later finalized-Task mutation"
    );

    Ok(())
}

async fn seed_connection_failure_detail_upgrade_fixture(
    store: &PgStore,
) -> Result<(), Box<dyn Error>> {
    let updated = sqlx::query(
        "UPDATE connection_operations \
         SET operation_state = 'failed', failure_category = 'bridge-gateway-http', \
             failure_detail = $2 \
         WHERE operation_id = $1::text::uuid",
    )
    .bind("00000000-0000-0000-0000-000000001057")
    .bind(serde_json::json!({
        "upstreamStatus": 400,
        "reason": "OAuth redirect target is not allowed"
    }))
    .execute(store.pool())
    .await?
    .rows_affected();
    assert_eq!(
        updated, 1,
        "the pre-0063 connection operation fixture must exist"
    );
    Ok(())
}

async fn assert_connection_failure_detail_upgrade_result(
    store: &PgStore,
) -> Result<(), Box<dyn Error>> {
    let operation_id = "00000000-0000-0000-0000-000000001057";
    let historical = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT failure_detail FROM connection_operations WHERE operation_id = $1::text::uuid",
    )
    .bind(operation_id)
    .fetch_one(store.pool())
    .await?;
    assert_eq!(
        historical,
        serde_json::json!({
            "upstreamStatus": 400,
            "reason": "OAuth redirect target is not allowed"
        }),
        "migration 0063 must preserve historical failure detail without inventing a code"
    );

    let with_code = serde_json::json!({
        "upstreamStatus": 400,
        "code": "oauth_redirect_target_not_allowed",
        "reason": "OAuth redirect target is not allowed"
    });
    sqlx::query(
        "UPDATE connection_operations SET failure_detail = $2 \
         WHERE operation_id = $1::text::uuid",
    )
    .bind(operation_id)
    .bind(&with_code)
    .execute(store.pool())
    .await?;
    assert_eq!(
        sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT failure_detail FROM connection_operations \
             WHERE operation_id = $1::text::uuid",
        )
        .bind(operation_id)
        .fetch_one(store.pool())
        .await?,
        with_code,
        "migration 0063 must admit the bounded MCP-GW code"
    );

    for invalid in [
        serde_json::json!({"upstreamStatus": 400, "code": "unsafe code"}),
        serde_json::json!({"upstreamStatus": 400, "code": "x".repeat(101)}),
        serde_json::json!({"upstreamStatus": 400, "code": "safe", "raw": "response"}),
    ] {
        assert!(
            sqlx::query(
                "UPDATE connection_operations SET failure_detail = $2 \
                 WHERE operation_id = $1::text::uuid",
            )
            .bind(operation_id)
            .bind(invalid)
            .execute(store.pool())
            .await
            .is_err(),
            "migration 0063 must reject unbounded or non-schema failure detail"
        );
    }
    Ok(())
}

async fn assert_github_repository_automation_upgrade_result(
    store: &PgStore,
) -> Result<(), Box<dyn Error>> {
    let operation_id = "00000000-0000-0000-0000-000000001057";
    let historical = sqlx::query_as::<_, (i64, String, String)>(
        "SELECT authority_version, authority_digest, mcp_gw_version \
         FROM connection_operations WHERE operation_id = $1::text::uuid",
    )
    .bind(operation_id)
    .fetch_one(store.pool())
    .await?;
    assert_eq!(
        historical,
        (
            2,
            "sha256:9a572bcefa75b6f2b5b4931d8604c1ad3f3e7560e0e0c2843646ec4f7853ef02".to_owned(),
            "0.4.9".to_owned(),
        ),
        "migration 0064 must preserve an existing v2 connection operation exactly"
    );

    let mut transaction = store.pool().begin().await?;
    let probes = [
        (
            "00000000-0000-0000-0000-000000000058",
            "00000000-0000-0000-0000-000000001058",
            1_i64,
            "sha256:7735d22e083daef4bdbd51bb63a652720ef06f5499422e7a8eef4930a6c58663",
            "0.3.2",
            "status",
        ),
        (
            "00000000-0000-0000-0000-000000000059",
            "00000000-0000-0000-0000-000000001059",
            2_i64,
            "sha256:9a572bcefa75b6f2b5b4931d8604c1ad3f3e7560e0e0c2843646ec4f7853ef02",
            "0.4.9",
            "status",
        ),
        (
            "00000000-0000-0000-0000-000000000060",
            "00000000-0000-0000-0000-000000001060",
            3_i64,
            "sha256:d5878c6ae538174c5e0c32ac6aa4617f4ac8e6787b1495b08bd5af9e48f7fbe3",
            "0.4.9",
            "status",
        ),
        (
            "00000000-0000-0000-0000-000000000061",
            "00000000-0000-0000-0000-000000001061",
            4_i64,
            "sha256:55e4c02ca61f399b105ac87195913092753c0e398f7b3ef4241478e3ffa99945",
            "0.4.9",
            "dispatch",
        ),
    ];
    for (task_uid, probe_operation_id, version, digest, gateway, operation_kind) in probes {
        sqlx::query(
            "INSERT INTO task_submissions (\
                 task_uid, idempotency_key, submitter_service, acting_user, acting_user_id, \
                 owner, owner_user_id, identity_binding_state, workflow, workflow_name, workflow_version, \
                 workflow_digest, user_envelope_instance_id, user_envelope_revision, \
                 user_envelope_digest, authority_kind, user_envelope_snapshot, internal_authority_id, \
                 internal_authority_version, internal_authority_digest, coding_agent_runtime, runtime_uid, \
                 runtime_namespace, runtime_name, runtime_ownership, phase, runtime_spec, \
                 finalize_requested, finalized, failure_reason, agent_command, execution_binding, \
                 direct_task_evidence, envelope_revision, orchestration_version, orchestration_operation_id, \
                 candidate_digest, service_envelope_digest, original_admission_decision, original_admission_deltas) \
             SELECT $1::text::uuid, $4, 'steward-connections', acting_user, acting_user_id, \
                    owner, owner_user_id, identity_binding_state, 'connections.github.status', \
                    NULL, NULL, NULL, NULL, NULL, NULL, 'internal', NULL, 'steward-connections', \
                    $2, $3, coding_agent_runtime, NULL, runtime_namespace, \
                    'task-migration-0064', runtime_ownership, 'failed', runtime_spec, true, true, \
                    'upgrade_fixture', agent_command, execution_binding, NULL, envelope_revision, \
                    orchestration_version, $5::text::uuid, candidate_digest, service_envelope_digest, \
                    original_admission_decision, original_admission_deltas \
             FROM task_submissions \
             WHERE task_uid = '00000000-0000-0000-0000-000000000003'",
        )
        .bind(task_uid)
        .bind(version)
        .bind(digest)
        .bind(format!("migration-0064-authority-v{version}"))
        .bind(probe_operation_id)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "INSERT INTO connection_operations (\
                 operation_id, task_uid, canonical_user_id, provider, operation_kind, \
                 submitter_service, authority_id, authority_version, authority_digest, \
                 runtime_spec_snapshot, command_snapshot, artifact_trust_mode, bridge_image_digest, \
                 mcp_gw_origin, mcp_gw_version, runtime_namespace, runtime_class, \
                 idempotency_identity, response_deadline_at) \
             VALUES (\
                 $1::text::uuid, $2::text::uuid, \
                 'usr_0123456789abcdef0123456789abcdef', 'github', $7, \
                 'steward-connections', 'steward-connections', $3, $4, '{}'::jsonb, \
                 '[]'::jsonb, 'github-attestation', \
                 'ghcr.io/example-org/connections-bridge@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', \
                 'https://gateway.example.test', $5, 'steward-workflows', '', \
                 $6, now() + interval '1 minute')",
        )
        .bind(probe_operation_id)
        .bind(task_uid)
        .bind(version)
        .bind(digest)
        .bind(gateway)
        .bind(format!("migration-0064-authority-v{version}"))
        .bind(operation_kind)
        .execute(&mut *transaction)
        .await?;
        if version < 4 {
            sqlx::query(
                "UPDATE connection_operations SET operation_state = 'succeeded' \
                 WHERE operation_id = $1::text::uuid",
            )
            .bind(probe_operation_id)
            .execute(&mut *transaction)
            .await?;
        }
    }

    for (operation_kind, expired_result) in [
        ("repositories", true),
        ("workflow", true),
        ("run_status", true),
        ("dispatch", false),
        ("publish", false),
    ] {
        let identity = format!("migration-0065-{operation_kind}");
        let publication_branch = (operation_kind == "publish").then_some(
            "steward/task-0123456789abcdef0123456789abcdef-fedcba9876543210fedcba9876543210",
        );
        insert_github_automation_retry_probe(
            &mut transaction,
            Uuid::new_v4(),
            operation_kind,
            &identity,
            publication_branch,
            expired_result,
        )
        .await?;
        insert_github_automation_retry_probe(
            &mut transaction,
            Uuid::new_v4(),
            operation_kind,
            &identity,
            publication_branch,
            expired_result,
        )
        .await?;
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*)::bigint FROM connection_operations \
                 WHERE canonical_user_id = 'usr_0123456789abcdef0123456789abcdef' \
                   AND provider = 'github' AND operation_kind = $1 \
                   AND idempotency_identity = $2",
            )
            .bind(operation_kind)
            .bind(&identity)
            .fetch_one(&mut *transaction)
            .await?,
            2,
            "migration 0065 must allow a fresh immutable row after an expired read result or failed write"
        );
    }

    let malformed_publication_branch = sqlx::query(
        "UPDATE connection_operations SET publication_branch = 'steward/task-invalid' \
         WHERE idempotency_identity = 'migration-0065-publish'",
    )
    .execute(&mut *transaction)
    .await;
    assert!(
        malformed_publication_branch.is_err(),
        "migration 0065 must reject malformed publication branch capabilities"
    );
    let branch_on_read = sqlx::query(
        "UPDATE connection_operations \
         SET publication_branch = 'steward/task-0123456789abcdef0123456789abcdef-fedcba9876543210fedcba9876543210' \
         WHERE idempotency_identity = 'migration-0065-repositories'",
    )
    .execute(&mut *transaction)
    .await;
    assert!(
        branch_on_read.is_err(),
        "migration 0065 must retain publication branches only for publish operations"
    );

    let v4_operation_id = "00000000-0000-0000-0000-000000001061";
    let downgraded = sqlx::query(
        "UPDATE connection_operations \
         SET authority_version = 3, \
             authority_digest = 'sha256:d5878c6ae538174c5e0c32ac6aa4617f4ac8e6787b1495b08bd5af9e48f7fbe3' \
         WHERE operation_id = $1::text::uuid",
    )
    .bind(v4_operation_id)
    .execute(&mut *transaction)
    .await;
    assert!(
        downgraded.is_err(),
        "a v1-v3 authority must never authorize a v4 repository operation"
    );
    transaction.rollback().await?;
    Ok(())
}

async fn insert_github_automation_retry_probe(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    operation_id: Uuid,
    operation_kind: &str,
    idempotency_identity: &str,
    publication_branch: Option<&str>,
    expired_result: bool,
) -> Result<(), Box<dyn Error>> {
    sqlx::query(
        "INSERT INTO task_submissions (\
             task_uid, idempotency_key, submitter_service, acting_user, acting_user_id, \
             owner, owner_user_id, identity_binding_state, workflow, workflow_name, workflow_version, \
             workflow_digest, user_envelope_instance_id, user_envelope_revision, \
             user_envelope_digest, authority_kind, user_envelope_snapshot, internal_authority_id, \
             internal_authority_version, internal_authority_digest, coding_agent_runtime, runtime_uid, \
             runtime_namespace, runtime_name, runtime_ownership, phase, runtime_spec, \
             finalize_requested, finalized, failure_reason, agent_command, execution_binding, \
             direct_task_evidence, envelope_revision, orchestration_version, orchestration_operation_id, \
             candidate_digest, service_envelope_digest, original_admission_decision, original_admission_deltas) \
         SELECT $1, 'migration-0065-' || $1::text, 'steward-connections', acting_user, acting_user_id, \
                owner, owner_user_id, identity_binding_state, 'connections.github.' || $2, \
                NULL, NULL, NULL, NULL, NULL, NULL, 'internal', NULL, 'steward-connections', \
                4, $3, coding_agent_runtime, NULL, runtime_namespace, \
                'task-migration-0065-' || replace($1::text, '-', ''), runtime_ownership, 'failed', \
                runtime_spec, true, true, 'upgrade_fixture', agent_command, execution_binding, NULL, \
                envelope_revision, orchestration_version, $1, candidate_digest, \
                service_envelope_digest, original_admission_decision, original_admission_deltas \
         FROM task_submissions \
         WHERE task_uid = '00000000-0000-0000-0000-000000000003'",
    )
    .bind(operation_id)
    .bind(operation_kind)
    .bind("sha256:55e4c02ca61f399b105ac87195913092753c0e398f7b3ef4241478e3ffa99945")
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO connection_operations (\
             operation_id, task_uid, canonical_user_id, provider, operation_kind, \
             submitter_service, authority_id, authority_version, authority_digest, \
             runtime_spec_snapshot, command_snapshot, artifact_trust_mode, bridge_image_digest, \
             mcp_gw_origin, mcp_gw_version, runtime_namespace, runtime_class, \
             idempotency_identity, publication_branch, operation_state, result, result_expires_at, \
             failure_category, finalization_state, cleanup_state, response_deadline_at) \
         VALUES ($1, $1, 'usr_0123456789abcdef0123456789abcdef', 'github', $2, \
             'steward-connections', 'steward-connections', 4, \
             'sha256:55e4c02ca61f399b105ac87195913092753c0e398f7b3ef4241478e3ffa99945', \
             '{}'::jsonb, '[]'::jsonb, 'github-attestation', \
             'ghcr.io/example-org/connections-bridge@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', \
             'https://gateway.example.test', '0.4.9', 'steward-workflows', '', $3, $4, \
             CASE WHEN $5 THEN 'succeeded' ELSE 'failed' END, \
             CASE WHEN $5 THEN '{}'::jsonb ELSE NULL END, \
             CASE WHEN $5 THEN now() - interval '1 second' ELSE NULL END, \
             CASE WHEN $5 THEN NULL ELSE 'upgrade_fixture' END, \
             'finalized', 'clean', now() + interval '1 minute')",
    )
    .bind(operation_id)
    .bind(operation_kind)
    .bind(idempotency_identity)
    .bind(publication_branch)
    .bind(expired_result)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn verify_source_provenance_byte_limits(store: &PgStore) -> Result<(), Box<dyn Error>> {
    let exact = serde_json::json!({
        "contractVersion": "steward.source-provenance/v1",
        "provider": "github",
        "repository": {
            "id": "123456",
            "ownerId": "7890",
            "name": "é".repeat(256),
        },
        "triggeredSha": format!("git:sha1:{}", "a".repeat(40)),
        "run": {
            "id": "900001",
            "attempt": 1,
        },
        "event": "é".repeat(256),
        "ref": "é".repeat(1024),
        "actorId": "24680",
        "actor": "é".repeat(256),
        "callerWorkflow": {
            "ref": "é".repeat(1024),
            "sha": format!("git:sha1:{}", "b".repeat(40)),
        },
        "reusableWorkflow": {
            "ref": "é".repeat(1024),
            "sha": format!("git:sha1:{}", "c".repeat(40)),
        },
    });
    let frozen_provenance: SourceProvenance = serde_json::from_value(exact.clone())?;
    frozen_provenance.validate().map_err(io::Error::other)?;

    let mut connection = store.pool().acquire().await?;
    sqlx::query(
        "CREATE TEMP TABLE source_provenance_byte_limit_probe \
         (LIKE task_submissions INCLUDING CONSTRAINTS)",
    )
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "DO $probe$ \
         DECLARE column_name text; \
         BEGIN \
           FOR column_name IN \
             SELECT attribute.attname \
             FROM pg_attribute attribute \
             WHERE attribute.attrelid = \
                     'pg_temp.source_provenance_byte_limit_probe'::regclass \
               AND attribute.attnum > 0 \
               AND NOT attribute.attisdropped \
               AND attribute.attname <> 'source_provenance' \
           LOOP \
             EXECUTE format( \
               'ALTER TABLE pg_temp.source_provenance_byte_limit_probe DROP COLUMN %I CASCADE', \
               column_name \
             ); \
           END LOOP; \
         END \
         $probe$",
    )
    .execute(&mut *connection)
    .await?;
    sqlx::query("INSERT INTO source_provenance_byte_limit_probe (source_provenance) VALUES ($1)")
        .bind(&exact)
        .execute(&mut *connection)
        .await?;

    for (field, pointer, value) in [
        ("repository.name", "/repository/name", "é".repeat(257)),
        ("event", "/event", "é".repeat(257)),
        ("ref", "/ref", "é".repeat(1025)),
        ("actor", "/actor", "é".repeat(257)),
        (
            "callerWorkflow.ref",
            "/callerWorkflow/ref",
            "é".repeat(1025),
        ),
        (
            "reusableWorkflow.ref",
            "/reusableWorkflow/ref",
            "é".repeat(1025),
        ),
    ] {
        let mut too_many_bytes = exact.clone();
        *too_many_bytes
            .pointer_mut(pointer)
            .ok_or_else(|| io::Error::other(format!("missing provenance field {field}")))? =
            serde_json::json!(value);
        assert!(
            serde_json::from_value::<SourceProvenance>(too_many_bytes.clone()).is_err(),
            "the frozen Rust contract must reject {field} above its UTF-8 byte limit"
        );
        assert!(
            sqlx::query(
                "INSERT INTO source_provenance_byte_limit_probe (source_provenance) VALUES ($1)",
            )
            .bind(&too_many_bytes)
            .execute(&mut *connection)
            .await
            .is_err(),
            "migration 0056 must reject {field} above its UTF-8 byte limit"
        );
    }

    Ok(())
}

async fn seed_connection_association_upgrade_fixture(
    store: &PgStore,
) -> Result<(), Box<dyn Error>> {
    let seeded_user = "usr_11111111111111111111111111111111";
    let replacement_user = "usr_22222222222222222222222222222222";
    for (user_id, email) in [
        (seeded_user, "carol@example.com"),
        (replacement_user, "dave@example.org"),
    ] {
        sqlx::query(
            "INSERT INTO canonical_users (user_id, organization_id, display_email) \
             VALUES ($1, 'org_example', $2)",
        )
        .bind(user_id)
        .bind(email)
        .execute(store.pool())
        .await?;
    }

    for (subject_id, observed_event_id, seeded_event_id, actor_id, current_user, revision) in [
        (
            "00000000-0000-0000-0000-000000000531",
            "00000000-0000-0000-0000-000000001531",
            "00000000-0000-0000-0000-000000002531",
            "531",
            replacement_user,
            3_i64,
        ),
        (
            "00000000-0000-0000-0000-000000000532",
            "00000000-0000-0000-0000-000000001532",
            "00000000-0000-0000-0000-000000002532",
            "532",
            seeded_user,
            2_i64,
        ),
    ] {
        sqlx::query(
            "INSERT INTO federated_subjects \
             (subject_id, issuer, subject, state, canonical_user_id, revision) \
             VALUES ($1::text::uuid, 'https://identity.example.test', $2, \
                     'associated', $3, $4)",
        )
        .bind(subject_id)
        .bind(format!("github-actions:actor:{actor_id}"))
        .bind(current_user)
        .bind(revision)
        .execute(store.pool())
        .await?;
        sqlx::query(
            "INSERT INTO federated_subject_audit \
             (event_id, subject_id, action, actor, previous_canonical_user_id, \
              canonical_user_id, previous_revision, revision) \
             VALUES ($2::text::uuid, $1::text::uuid, 'observed', 'task-auth', \
                     NULL, NULL, 0, 1), \
                    ($3::text::uuid, $1::text::uuid, 'v2_seeded', 'steward-task-v2', \
                     NULL, $4, 1, 2)",
        )
        .bind(subject_id)
        .bind(observed_event_id)
        .bind(seeded_event_id)
        .bind(seeded_user)
        .execute(store.pool())
        .await?;
    }
    sqlx::query(
        "INSERT INTO federated_subject_audit \
         (event_id, subject_id, action, actor, previous_canonical_user_id, \
          canonical_user_id, previous_revision, revision) \
         VALUES ('00000000-0000-0000-0000-000000003531', \
                 '00000000-0000-0000-0000-000000000531', 'replaced', \
                 'identity-admin', $1, $2, 2, 3)",
    )
    .bind(seeded_user)
    .bind(replacement_user)
    .execute(store.pool())
    .await?;
    Ok(())
}

async fn assert_connection_association_upgrade_result(
    store: &PgStore,
) -> Result<(), Box<dyn Error>> {
    let rows = sqlx::query_as::<_, (String, String)>(
        "SELECT subject, association_method \
         FROM federated_subjects \
         WHERE subject IN ('github-actions:actor:531', 'github-actions:actor:532') \
         ORDER BY subject",
    )
    .fetch_all(store.pool())
    .await?;
    assert_eq!(
        rows,
        [
            ("github-actions:actor:531".to_owned(), "admin".to_owned()),
            ("github-actions:actor:532".to_owned(), "v2-claim".to_owned(),),
        ],
        "migration 0053 must classify the current association transition, not historical proof"
    );
    Ok(())
}

async fn verify_direct_connection_auto_association(store: &PgStore) -> Result<(), Box<dyn Error>> {
    let issuer = "https://identity.example.test";
    let alice =
        CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef").map_err(io::Error::other)?;
    let bob =
        CanonicalUserId::parse("usr_abcdef0123456789abcdef0123456789").map_err(io::Error::other)?;

    let linked = direct_connection_status(store, Some(issuer), &alice, "7001").await?;
    assert_eq!(linked.github_actions_identity_linked, Some(true));
    assert_eq!(
        store
            .resolve_federated_subject(issuer, "github-actions:actor:7001")
            .await?
            .user_id,
        alice,
        "a verified Connect status must make the first v3 subject resolution succeed"
    );

    let manual = direct_connection_status(store, None, &alice, "7002").await?;
    assert!(manual.github_actions_identity_linked.is_none());
    assert!(
        store
            .federated_subject_by_external_identity(issuer, "github-actions:actor:7002")
            .await?
            .is_none(),
        "the manual-review setting must not create or associate a subject"
    );

    let conflict = FederatedSubjectObservation {
        issuer,
        subject: "github-actions:actor:7003",
        actor_login: Some("bob-gh"),
        display_name: Some("bob@example.org"),
    };
    store
        .associate_federated_subject_from_connection(conflict, &bob, "github", "7003")
        .await?;
    assert_eq!(
        direct_connection_status(store, Some(issuer), &alice, "7003")
            .await?
            .github_actions_identity_linked,
        Some(false),
        "Connect must report but never replace an association owned by another user"
    );

    let disabled = store
        .associate_federated_subject_from_connection(
            FederatedSubjectObservation {
                issuer,
                subject: "github-actions:actor:7004",
                actor_login: Some("bob-gh"),
                display_name: Some("bob@example.org"),
            },
            &bob,
            "github",
            "7004",
        )
        .await?;
    store
        .disable_federated_subject(FederatedSubjectDisable {
            subject_id: disabled.subject_id,
            expected_revision: disabled.revision,
            actor: alice.as_str(),
            reason: Some("PROJ-123 manual review"),
        })
        .await?;
    assert_eq!(
        direct_connection_status(store, Some(issuer), &alice, "7004")
            .await?
            .github_actions_identity_linked,
        Some(false),
        "Connect must report but never re-enable a disabled subject"
    );
    Ok(())
}

async fn direct_connection_status(
    store: &PgStore,
    issuer: Option<&str>,
    canonical_user_id: &CanonicalUserId,
    account_id: &str,
) -> Result<ProviderConnectionStatus, Box<dyn Error>> {
    let mint = Router::new().route(
        "/control-plane/token",
        post(|| async {
            Json(serde_json::json!({
                "access_token": "aaa.bbb.ccc",
                "expires_in": 15,
                "scope": "connections_status",
                "token_type": "Bearer"
            }))
        }),
    );
    let account_id = account_id.to_owned();
    let gateway = Router::new().route(
        "/connections/github/status",
        get(move || {
            let account_id = account_id.clone();
            async move {
                Json(serde_json::json!({
                    "version": "2",
                    "provider": "github",
                    "phase": "connected",
                    "connected": true,
                    "account": {
                        "provider": "github",
                        "id": account_id,
                        "login": "mutable-login",
                        "displayName": "alice@example.com"
                    },
                    "requiredScopes": ["repo"],
                    "grantedScopes": ["repo"],
                    "missingScopes": [],
                    "activeCredentialExpiresAt": null,
                    "renewalCredentialExpiresAt": null,
                    "lastAuthorizedAt": null,
                    "lastRenewedAt": null,
                    "lastValidatedAt": null,
                    "capabilities": {"interactiveAuthorization": true}
                }))
            }
        }),
    );
    let (mint_origin, _mint_guard) = serve_http(mint).await?;
    let (gateway_origin, _gateway_guard) = serve_http(gateway).await?;
    let credential_path = env::temp_dir().join(format!(
        "steward-connection-status-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    std::fs::write(&credential_path, "control-plane-fixture")?;
    let _credential_guard = TemporaryFile(credential_path.clone());
    let reader = DirectConnectionStatusReader::new(
        store.clone(),
        DirectConnectionStatusConfig {
            control_plane_credential_file: credential_path,
            mint_origin,
            federated_subject_issuer: issuer.map(str::to_owned),
        },
        &gateway_origin,
        "0.4.9",
    )
    .map_err(|error| {
        io::Error::other(format!("direct status reader rejected fixture: {error:?}"))
    })?;
    let broker = SplitConnectionsBroker::new(NoopConnectionMutations, reader);
    broker
        .status(&ConnectionSession {
            subject: ConnectionSubject {
                canonical_user_id: canonical_user_id.clone(),
                display_email: "alice@example.com".to_owned(),
            },
            binding: "browser-session".to_owned(),
        })
        .await
        .map_err(|error| io::Error::other(format!("direct status failed: {error:?}")))
        .map_err(Into::into)
}

async fn serve_http(router: Router) -> Result<(String, ServerGuard), Box<dyn Error>> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .map_err(io::Error::other)
    });
    Ok((format!("http://{address}"), ServerGuard(server)))
}

async fn historical_task_identity_snapshot(
    store: &PgStore,
) -> Result<serde_json::Value, Box<dyn Error>> {
    Ok(sqlx::query_scalar(
        "SELECT jsonb_build_object(\
             'tasks', (\
                 SELECT jsonb_agg(to_jsonb(task_row) ORDER BY task_row.task_uid) \
                 FROM task_submissions task_row\
             ), \
             'runs', (\
                 SELECT jsonb_agg(to_jsonb(event_row) ORDER BY event_row.id) \
                 FROM task_lifecycle_events event_row\
             ), \
             'canonicalUsers', (\
                 SELECT jsonb_agg(to_jsonb(user_row) ORDER BY user_row.user_id) \
                 FROM canonical_users user_row\
             )\
         )",
    )
    .fetch_one(store.pool())
    .await?)
}

async fn verify_federated_subject_lifecycle(store: &PgStore) -> Result<(), Box<dyn Error>> {
    let observation = || FederatedSubjectObservation {
        issuer: "https://identity.example.test",
        subject: "github-actions:actor:16106037",
        actor_login: Some("alice-gh"),
        display_name: Some("Alice"),
    };
    let (first, second) = tokio::join!(
        store.observe_federated_subject(observation()),
        store.observe_federated_subject(observation()),
    );
    let first = first?;
    let second = second?;
    assert_eq!(first.subject_id, second.subject_id);
    assert_eq!(first.state, FederatedSubjectState::Observed);
    assert!(first.association_method.is_none());
    assert_eq!(first.revision, 1);
    assert_eq!(
        store
            .federated_subject_audit(first.subject_id)
            .await?
            .iter()
            .map(|event| event.action)
            .collect::<Vec<_>>(),
        [FederatedSubjectAuditAction::Observed],
        "concurrent first observation must converge on one subject and one audit fact"
    );
    assert!(matches!(
        store
            .resolve_federated_subject(&first.issuer, &first.subject)
            .await,
        Err(StoreError::FederatedSubjectUnassociated)
    ));

    let alice =
        CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef").map_err(io::Error::other)?;
    let (seeded_first, seeded_second) = tokio::join!(
        store.seed_federated_subject_association(observation(), &alice, "steward-task-v2"),
        store.seed_federated_subject_association(observation(), &alice, "steward-task-v2"),
    );
    let seeded_first = seeded_first?;
    let seeded_second = seeded_second?;
    assert_eq!(seeded_first.subject_id, seeded_second.subject_id);
    assert_eq!(seeded_first.state, seeded_second.state);
    assert_eq!(
        seeded_first.canonical_user_id,
        seeded_second.canonical_user_id
    );
    assert_eq!(seeded_first.revision, seeded_second.revision);
    assert_eq!(seeded_first.state, FederatedSubjectState::Associated);
    assert_eq!(seeded_first.canonical_user_id.as_ref(), Some(&alice));
    assert_eq!(
        seeded_first.association_method,
        Some(FederatedSubjectAssociationMethod::V2Claim)
    );
    assert_eq!(seeded_first.revision, 2);
    assert_eq!(
        store
            .federated_subject_audit(first.subject_id)
            .await?
            .iter()
            .map(|event| event.action)
            .collect::<Vec<_>>(),
        [
            FederatedSubjectAuditAction::Observed,
            FederatedSubjectAuditAction::V2Seeded,
        ],
        "concurrent v2 seeding must create exactly one association fact"
    );
    let resolved = store
        .resolve_federated_subject(&first.issuer, &first.subject)
        .await?;
    assert_eq!(resolved.user_id, alice);
    assert_eq!(resolved.display_email.as_str(), "alice@example.com");

    let bob =
        CanonicalUserId::parse("usr_abcdef0123456789abcdef0123456789").map_err(io::Error::other)?;
    sqlx::query(
        "INSERT INTO canonical_users (user_id, organization_id, display_email) \
         VALUES ($1, 'org_example', 'bob@example.org')",
    )
    .bind(bob.as_str())
    .execute(store.pool())
    .await?;
    let replaced = store
        .replace_federated_subject_association(FederatedSubjectAssociation {
            subject_id: first.subject_id,
            expected_revision: 2,
            canonical_user_id: &bob,
            actor: "usr_0123456789abcdef0123456789abcdef",
        })
        .await?;
    assert_eq!(replaced.canonical_user_id.as_ref(), Some(&bob));
    assert_eq!(
        replaced.association_method,
        Some(FederatedSubjectAssociationMethod::Admin)
    );
    assert_eq!(replaced.revision, 3);
    sqlx::query(
        "UPDATE canonical_users SET display_email = 'bob.updated@example.org' WHERE user_id = $1",
    )
    .bind(bob.as_str())
    .execute(store.pool())
    .await?;
    assert_eq!(
        store
            .resolve_federated_subject(&first.issuer, &first.subject)
            .await?
            .display_email
            .as_str(),
        "bob.updated@example.org",
        "federated resolution must use the canonical store's current display identity"
    );
    assert!(matches!(
        store
            .replace_federated_subject_association(FederatedSubjectAssociation {
                subject_id: first.subject_id,
                expected_revision: 2,
                canonical_user_id: &alice,
                actor: "usr_0123456789abcdef0123456789abcdef",
            })
            .await,
        Err(StoreError::FederatedSubjectConflict)
    ));
    let disabled = store
        .disable_federated_subject(FederatedSubjectDisable {
            subject_id: first.subject_id,
            expected_revision: 3,
            actor: "usr_0123456789abcdef0123456789abcdef",
            reason: Some("access revoked"),
        })
        .await?;
    assert_eq!(disabled.state, FederatedSubjectState::Disabled);
    assert_eq!(disabled.revision, 4);
    assert!(matches!(
        store
            .resolve_federated_subject(&first.issuer, &first.subject)
            .await,
        Err(StoreError::FederatedSubjectDisabled)
    ));
    assert!(matches!(
        store
            .seed_federated_subject_association(observation(), &bob, "steward-task-v2")
            .await,
        Err(StoreError::FederatedSubjectDisabled)
    ));
    assert!(matches!(
        store
            .associate_federated_subject_from_connection(observation(), &bob, "github", "16106037",)
            .await,
        Err(StoreError::FederatedSubjectDisabled)
    ));

    let observed_from_task = store
        .observe_federated_subject(FederatedSubjectObservation {
            issuer: "https://identity.example.test",
            subject: "github-actions:actor:424241",
            actor_login: Some("signed-login"),
            display_name: Some("Signed Display"),
        })
        .await?;
    let linked_observed = store
        .associate_federated_subject_from_connection(
            FederatedSubjectObservation {
                issuer: "https://identity.example.test",
                subject: "github-actions:actor:424241",
                actor_login: Some("connection-login"),
                display_name: Some("connection@example.org"),
            },
            &alice,
            "github",
            "424241",
        )
        .await?;
    assert_eq!(
        linked_observed.last_seen_at,
        observed_from_task.last_seen_at
    );
    assert_eq!(linked_observed.actor_login, observed_from_task.actor_login);
    assert_eq!(
        linked_observed.display_name,
        observed_from_task.display_name
    );

    let connected_account = FederatedSubjectObservation {
        issuer: "https://identity.example.test",
        subject: "github-actions:actor:424242",
        actor_login: Some("alice-gh"),
        display_name: Some("alice@example.com"),
    };
    let connected = store
        .associate_federated_subject_from_connection(connected_account, &alice, "github", "424242")
        .await?;
    assert_eq!(connected.state, FederatedSubjectState::Associated);
    assert_eq!(connected.canonical_user_id.as_ref(), Some(&alice));
    assert_eq!(
        connected.association_method,
        Some(FederatedSubjectAssociationMethod::ConnectionVerification)
    );
    let connected_audit = store.federated_subject_audit(connected.subject_id).await?;
    assert_eq!(
        connected_audit
            .iter()
            .map(|event| event.action)
            .collect::<Vec<_>>(),
        [
            FederatedSubjectAuditAction::Observed,
            FederatedSubjectAuditAction::ConnectionVerified,
        ]
    );
    let verification = connected_audit
        .last()
        .ok_or_else(|| io::Error::other("connection verification audit is missing"))?;
    assert_eq!(verification.actor, "connection-verification");
    assert_eq!(verification.connection_provider.as_deref(), Some("github"));
    assert_eq!(
        verification.connection_account_id.as_deref(),
        Some("424242")
    );
    let repeated = store
        .associate_federated_subject_from_connection(
            FederatedSubjectObservation {
                issuer: "https://identity.example.test",
                subject: "github-actions:actor:424242",
                actor_login: Some("alice-renamed"),
                display_name: Some("changed@example.org"),
            },
            &alice,
            "github",
            "424242",
        )
        .await?;
    assert_eq!(repeated.updated_at, connected.updated_at);
    assert_eq!(repeated.last_seen_at, connected.last_seen_at);
    assert_eq!(repeated.actor_login, connected.actor_login);
    assert_eq!(repeated.display_name, connected.display_name);
    assert_eq!(
        store.federated_subject_audit(connected.subject_id).await?,
        connected_audit,
        "repeated status reads must not write subject or audit state"
    );
    assert!(matches!(
        store
            .associate_federated_subject_from_connection(
                FederatedSubjectObservation {
                    issuer: "https://identity.example.test",
                    subject: "github-actions:actor:999999",
                    actor_login: Some("alice-gh"),
                    display_name: Some("alice@example.com"),
                },
                &alice,
                "github",
                "424242",
            )
            .await,
        Err(StoreError::InvalidFederatedSubject)
    ));
    assert!(
        store
            .federated_subject_by_external_identity(
                "https://identity.example.test",
                "github-actions:actor:999999",
            )
            .await?
            .is_none(),
        "a connection for account A must not create or associate account B's subject"
    );
    assert!(matches!(
        store
            .associate_federated_subject_from_connection(
                FederatedSubjectObservation {
                    issuer: "https://identity.example.test",
                    subject: "github-actions:actor:424242",
                    actor_login: Some("alice-renamed"),
                    display_name: Some("bob@example.org"),
                },
                &bob,
                "github",
                "424242",
            )
            .await,
        Err(StoreError::FederatedSubjectConflict)
    ));
    assert_eq!(
        store
            .resolve_federated_subject(
                "https://identity.example.test",
                "github-actions:actor:424242",
            )
            .await?
            .user_id,
        alice,
        "mutable login or display metadata must never move a numeric GitHub account association"
    );

    let competing_barrier = Arc::new(Barrier::new(3));
    let alice_store = store.clone();
    let alice_id = alice.clone();
    let alice_barrier = Arc::clone(&competing_barrier);
    let alice_attempt = tokio::spawn(async move {
        alice_barrier.wait().await;
        alice_store
            .associate_federated_subject_from_connection(
                FederatedSubjectObservation {
                    issuer: "https://identity.example.test",
                    subject: "github-actions:actor:525252",
                    actor_login: Some("alice-race"),
                    display_name: Some("alice@example.com"),
                },
                &alice_id,
                "github",
                "525252",
            )
            .await
    });
    let bob_store = store.clone();
    let bob_id = bob.clone();
    let bob_barrier = Arc::clone(&competing_barrier);
    let bob_attempt = tokio::spawn(async move {
        bob_barrier.wait().await;
        bob_store
            .associate_federated_subject_from_connection(
                FederatedSubjectObservation {
                    issuer: "https://identity.example.test",
                    subject: "github-actions:actor:525252",
                    actor_login: Some("bob-race"),
                    display_name: Some("bob@example.org"),
                },
                &bob_id,
                "github",
                "525252",
            )
            .await
    });
    competing_barrier.wait().await;
    let alice_result = alice_attempt.await?;
    let bob_result = bob_attempt.await?;
    let (winning_user, winning_login, winner) = match (&alice_result, &bob_result) {
        (Ok(record), Err(StoreError::FederatedSubjectConflict)) => (&alice, "alice-race", record),
        (Err(StoreError::FederatedSubjectConflict), Ok(record)) => (&bob, "bob-race", record),
        outcomes => panic!("exactly one competing connection owner must win: {outcomes:?}"),
    };
    assert_eq!(winner.canonical_user_id.as_ref(), Some(winning_user));
    assert_eq!(winner.actor_login.as_deref(), Some(winning_login));
    let competing_audit = store.federated_subject_audit(winner.subject_id).await?;
    assert_eq!(competing_audit.len(), 2);
    assert_eq!(
        competing_audit.last().map(|event| event.action),
        Some(FederatedSubjectAuditAction::ConnectionVerified)
    );

    let disable_race_subject = store
        .observe_federated_subject(FederatedSubjectObservation {
            issuer: "https://identity.example.test",
            subject: "github-actions:actor:525253",
            actor_login: Some("signed-race"),
            display_name: Some("Signed Race"),
        })
        .await?;
    let disable_race_subject_id = disable_race_subject.subject_id;
    let disable_race_revision = disable_race_subject.revision;
    let disable_race_last_seen_at = disable_race_subject.last_seen_at.clone();
    let disable_barrier = Arc::new(Barrier::new(3));
    let associate_store = store.clone();
    let associate_user = alice.clone();
    let associate_barrier = Arc::clone(&disable_barrier);
    let associate_attempt = tokio::spawn(async move {
        associate_barrier.wait().await;
        associate_store
            .associate_federated_subject_from_connection(
                FederatedSubjectObservation {
                    issuer: "https://identity.example.test",
                    subject: "github-actions:actor:525253",
                    actor_login: Some("connection-race"),
                    display_name: Some("connection@example.org"),
                },
                &associate_user,
                "github",
                "525253",
            )
            .await
    });
    let disable_store = store.clone();
    let disable_barrier_task = Arc::clone(&disable_barrier);
    let disable_attempt = tokio::spawn(async move {
        disable_barrier_task.wait().await;
        disable_store
            .disable_federated_subject(FederatedSubjectDisable {
                subject_id: disable_race_subject_id,
                expected_revision: disable_race_revision,
                actor: "usr_0123456789abcdef0123456789abcdef",
                reason: Some("PROJ-123 concurrent revocation"),
            })
            .await
    });
    disable_barrier.wait().await;
    let associate_result = associate_attempt.await?;
    let disable_result = disable_attempt.await?;
    assert!(
        matches!(
            (&associate_result, &disable_result),
            (Ok(_), Err(StoreError::FederatedSubjectConflict))
                | (Err(StoreError::FederatedSubjectDisabled), Ok(_))
        ),
        "association and disable race must have exactly one winner: {associate_result:?}, {disable_result:?}"
    );
    let disable_race_final = store
        .federated_subject_by_external_identity(
            "https://identity.example.test",
            "github-actions:actor:525253",
        )
        .await?
        .ok_or_else(|| io::Error::other("disable race subject disappeared"))?;
    assert_eq!(
        disable_race_final.actor_login.as_deref(),
        Some("signed-race")
    );
    assert_eq!(
        disable_race_final.display_name.as_deref(),
        Some("Signed Race")
    );
    assert_eq!(disable_race_final.last_seen_at, disable_race_last_seen_at);
    assert_eq!(
        store
            .federated_subject_audit(disable_race_final.subject_id)
            .await?
            .len(),
        2,
        "the losing transition must not append audit state"
    );

    let unlink_candidate = store
        .observe_federated_subject(FederatedSubjectObservation {
            issuer: "https://identity.example.test",
            subject: "github-actions:actor:8675309",
            actor_login: Some("alice-unlink"),
            display_name: Some("Alice Unlink"),
        })
        .await?;
    let linked = store
        .associate_federated_subject(FederatedSubjectAssociation {
            subject_id: unlink_candidate.subject_id,
            expected_revision: unlink_candidate.revision,
            canonical_user_id: &alice,
            actor: bob.as_str(),
        })
        .await?;
    assert!(
        store
            .federated_subjects_for_canonical_user(&alice)
            .await?
            .iter()
            .any(|subject| subject.subject_id == linked.subject_id)
    );
    let unlinked = store
        .unlink_federated_subject(FederatedSubjectUnlink {
            subject_id: linked.subject_id,
            expected_revision: linked.revision,
            canonical_user_id: &alice,
            actor: bob.as_str(),
        })
        .await?;
    assert_eq!(unlinked.state, FederatedSubjectState::Observed);
    assert!(unlinked.canonical_user_id.is_none());
    assert!(unlinked.association_method.is_none());
    assert!(
        !store
            .federated_subjects_for_canonical_user(&alice)
            .await?
            .iter()
            .any(|subject| subject.subject_id == linked.subject_id)
    );
    assert!(matches!(
        store
            .resolve_federated_subject(&unlinked.issuer, &unlinked.subject)
            .await,
        Err(StoreError::FederatedSubjectUnassociated)
    ));
    assert_eq!(
        store
            .federated_subject_audit(unlinked.subject_id)
            .await?
            .iter()
            .map(|event| event.action)
            .collect::<Vec<_>>(),
        [
            FederatedSubjectAuditAction::Observed,
            FederatedSubjectAuditAction::Associated,
            FederatedSubjectAuditAction::Unassociated,
        ]
    );
    assert!(matches!(
        store
            .unlink_federated_subject(FederatedSubjectUnlink {
                subject_id: linked.subject_id,
                expected_revision: linked.revision,
                canonical_user_id: &alice,
                actor: bob.as_str(),
            })
            .await,
        Err(StoreError::FederatedSubjectConflict)
    ));
    let relinked = store
        .associate_federated_subject(FederatedSubjectAssociation {
            subject_id: unlinked.subject_id,
            expected_revision: unlinked.revision,
            canonical_user_id: &alice,
            actor: bob.as_str(),
        })
        .await?;
    assert_eq!(relinked.state, FederatedSubjectState::Associated);

    let similarity = store
        .observe_federated_subject(FederatedSubjectObservation {
            issuer: "https://identity.example.test",
            subject: "github-actions:actor:27182818",
            actor_login: Some("alice"),
            display_name: Some("alice@example.com"),
        })
        .await?;
    assert_eq!(similarity.state, FederatedSubjectState::Observed);
    assert!(similarity.canonical_user_id.is_none());
    assert!(matches!(
        store
            .resolve_federated_subject(&similarity.issuer, &similarity.subject)
            .await,
        Err(StoreError::FederatedSubjectUnassociated)
    ));
    let exact_lookup = store
        .federated_subject_by_external_identity(&similarity.issuer, &similarity.subject)
        .await?
        .ok_or_else(|| io::Error::other("exact federated-subject lookup returned no record"))?;
    assert_eq!(exact_lookup.subject_id, similarity.subject_id);
    assert!(
        store
            .federated_subject_by_external_identity(
                &similarity.issuer,
                "github-actions:actor:31415926"
            )
            .await?
            .is_none(),
        "exact lookup must not infer a subject from display metadata or a similar key"
    );

    assert!(
        sqlx::query("UPDATE federated_subject_audit SET actor = 'tampered' WHERE subject_id = $1",)
            .bind(first.subject_id)
            .execute(store.pool())
            .await
            .is_err(),
        "federated-subject audit must reject mutation"
    );
    Ok(())
}

async fn seed_v0123_upgrade_fixture(store: &PgStore) -> Result<(), Box<dyn Error>> {
    let user_id = "usr_0123456789abcdef0123456789abcdef";
    let user_digest = format!("sha256:{}", "b".repeat(64));
    let service_digest = format!("sha256:{}", "c".repeat(64));
    let internal_digest = format!("sha256:{}", "d".repeat(64));
    let workflow_digest = format!("sha256:{}", "e".repeat(64));
    let candidate_digest = format!("sha256:{}", "f".repeat(64));
    let approved_envelope = serde_json::json!({
        "revision": 7,
        "spec": {
            "llms": [{"provider": "provider-a", "model": "model-a"}],
            "tools": [],
            "budget": {"monthlyLimit": "100.00", "currency": "USD"},
            "ttl": "24h",
            "runner": {"platforms": ["linux"], "architectures": []}
        }
    });
    let runtime_spec = serde_json::json!({
        "canonicalAuthority": {
            "schemaVersion": "steward/canonical-authority-binding/v1",
            "ownerUserId": user_id,
            "actingUserId": user_id
        }
    });

    sqlx::query(
        "INSERT INTO canonical_users \
         (user_id, organization_id, display_email) VALUES ($1, 'org_example', 'alice@example.com')",
    )
    .bind(user_id)
    .execute(store.pool())
    .await?;
    sqlx::query(
        "INSERT INTO workflow_revisions \
         (name, version, display_name, agent, prompt, content_digest, published_by) \
         VALUES ('release-summary', 1, 'Release summary', 'agent@1', 'Summarize.', $1, 'admin')",
    )
    .bind(&workflow_digest)
    .execute(store.pool())
    .await?;
    sqlx::query(
        "INSERT INTO envelopes (scope_kind, scope_ref, revision, spec, authored_by) \
         VALUES ('member_role', 'engineer', 7, $1, 'upgrade-admin')",
    )
    .bind(&approved_envelope["spec"])
    .execute(store.pool())
    .await?;
    let envelope_request_id = "00000000-0000-0000-0000-000000000100";
    sqlx::query(
        "INSERT INTO envelope_requests \
         (id, owner_user_id, template_id, template_revision, requested_envelope, idempotency_key) \
         VALUES ($1::text::uuid, $2, 'engineer', 7, $3, 'upgrade-envelope')",
    )
    .bind(envelope_request_id)
    .bind(user_id)
    .bind(&approved_envelope)
    .execute(store.pool())
    .await?;
    sqlx::query(
        "INSERT INTO envelope_request_events \
         (request_id, status, envelope_instance_id, envelope_digest, approved_envelope) \
         VALUES ($1::text::uuid, 'provisioned', 'envelope-instance-7', $2, $3)",
    )
    .bind(envelope_request_id)
    .bind(&user_digest)
    .bind(&approved_envelope)
    .execute(store.pool())
    .await?;

    for (task_number, idempotency_key, workflow, finalized, authority) in [
        (1_u128, "legacy-unfinished", "legacy-smoke", false, "legacy"),
        (2, "user-unfinished", "release-summary@1", false, "user"),
        (
            3,
            "internal-unfinished",
            "connections.github.status",
            false,
            "internal",
        ),
        (4, "legacy-terminal", "legacy-history", true, "legacy"),
    ] {
        let task_uid = format!("00000000-0000-0000-0000-{task_number:012}");
        let operation_id = format!("00000000-0000-0000-0000-{:012}", task_number + 1000);
        let mut transaction = store.pool().begin().await?;
        sqlx::query(
            "INSERT INTO task_submissions (\
                 task_uid, idempotency_key, submitter_service, acting_user, owner, \
                 workflow, coding_agent_runtime, runtime_namespace, runtime_name, \
                 runtime_ownership, phase, runtime_spec, agent_command, \
                 finalize_requested, finalized, acting_user_id, owner_user_id, \
                 identity_binding_state, workflow_name, workflow_version, workflow_digest, \
                 user_envelope_instance_id, user_envelope_revision, user_envelope_digest, \
                 internal_authority_id, internal_authority_version, internal_authority_digest, \
                 execution_binding, orchestration_version, orchestration_operation_id, \
                 candidate_digest, service_envelope_digest, original_admission_decision, \
                 original_admission_deltas\
             ) VALUES (\
                 $1::text::uuid, $2, 'steward-run', 'alice@example.com', 'alice@example.com', \
                 $3, 'agent@1', 'steward-workflows', $4, 'provisioned', $5, $6, \
                 '[\"agent\"]'::jsonb, $7, $7, $8, $8, 'bound', \
                 $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, 2, $19::text::uuid, \
                 $20, $21, 'admit', '[]'::jsonb\
             )",
        )
        .bind(&task_uid)
        .bind(idempotency_key)
        .bind(workflow)
        .bind(format!("task-{task_number}"))
        .bind(if finalized { "succeeded" } else { "submitted" })
        .bind(&runtime_spec)
        .bind(finalized)
        .bind(user_id)
        .bind((authority == "user").then_some("release-summary"))
        .bind((authority == "user").then_some(1_i64))
        .bind((authority == "user").then_some(workflow_digest.as_str()))
        .bind((authority == "user").then_some("envelope-instance-7"))
        .bind((authority == "user").then_some(7_i64))
        .bind((authority == "user").then_some(user_digest.as_str()))
        .bind((authority == "internal").then_some("connections.github.status"))
        .bind((authority == "internal").then_some(1_i64))
        .bind((authority == "internal").then_some(internal_digest.as_str()))
        .bind((authority == "user").then_some(serde_json::json!({})))
        .bind(&operation_id)
        .bind(&candidate_digest)
        .bind(&service_digest)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "INSERT INTO task_runtime_operations (\
                 task_uid, operation_id, state, runtime_ownership, runtime_namespace, \
                 runtime_name, inert_manifest_digest, active_manifest_digest\
             ) VALUES (\
                 $1::text::uuid, $2::text::uuid, 'intent_recorded', 'provisioned', \
                 'steward-workflows', $3, $4, $4\
             )",
        )
        .bind(&task_uid)
        .bind(&operation_id)
        .bind(format!("task-{task_number}"))
        .bind(&candidate_digest)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
    }
    Ok(())
}

async fn assert_template_catalog_upgrade_result(store: &PgStore) -> Result<(), Box<dyn Error>> {
    let migrated_template = sqlx::query_as::<_, (String, Vec<String>, i64)>(
        "SELECT display_name, member_roles, revision \
         FROM envelope_template_revisions \
         WHERE template_id = 'engineer' AND revision = 7",
    )
    .fetch_one(store.pool())
    .await?;
    assert_eq!(
        migrated_template,
        ("engineer".to_owned(), vec!["engineer".to_owned()], 7),
        "the role-keyed template history must migrate without changing request identity"
    );
    let preserved_request = sqlx::query_as::<_, (String, i64)>(
        "SELECT template_id, template_revision FROM envelope_requests \
         WHERE idempotency_key = 'upgrade-envelope'",
    )
    .fetch_one(store.pool())
    .await?;
    assert_eq!(preserved_request, ("engineer".to_owned(), 7));

    Ok(())
}

async fn assert_v02_upgrade_result(store: &PgStore) -> Result<(), Box<dyn Error>> {
    let user = sqlx::query_as::<
        _,
        (
            i16,
            Option<String>,
            Option<serde_json::Value>,
            Option<String>,
        ),
    >(
        "SELECT orchestration_version, authority_kind, user_envelope_snapshot, \
                service_envelope_digest \
         FROM task_submissions WHERE idempotency_key = 'user-unfinished'",
    )
    .fetch_one(store.pool())
    .await?;
    assert_eq!(user.0, 3);
    assert_eq!(user.1.as_deref(), Some("user-envelope"));
    assert_eq!(
        user.2
            .and_then(|snapshot| snapshot.get("revision").cloned()),
        Some(7.into())
    );
    assert!(user.3.is_none());

    let internal = sqlx::query_as::<_, (i16, Option<String>, Option<String>)>(
        "SELECT orchestration_version, authority_kind, service_envelope_digest \
         FROM task_submissions WHERE idempotency_key = 'internal-unfinished'",
    )
    .fetch_one(store.pool())
    .await?;
    assert_eq!(internal.0, 3);
    assert_eq!(internal.1.as_deref(), Some("internal"));
    assert!(internal.2.is_none());

    let historical = sqlx::query_as::<_, (i16, Option<String>, bool)>(
        "SELECT orchestration_version, authority_kind, finalized \
         FROM task_submissions WHERE idempotency_key = 'legacy-terminal'",
    )
    .fetch_one(store.pool())
    .await?;
    assert_eq!(historical, (2, None, true));
    Ok(())
}
