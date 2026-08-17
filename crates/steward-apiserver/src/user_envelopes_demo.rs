//! Loopback-only user-envelope hand-test fixture. It contains no provider credential and is
//! deliberately separate from the production PgStore-backed broker contract.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::response::Redirect;
use axum::routing::get;
use sha2::{Digest, Sha256};
use steward_admission::{Envelope, EnvelopeSpec};
use steward_types::{Budget, CanonicalUserId, Duration, ModelRef, ToolGrant};
use uuid::Uuid;

use crate::BoxFuture;
use crate::browser_auth::{
    BrowserSessionBinding, LocalFakeIdentity, browser_auth_router, local_fake_browser_auth_service,
    protect_browser_routes,
};
use crate::user_envelopes::{
    AvailableEnvelopeTemplate, ConnectionReadiness, EnvelopeRequestBroker,
    EnvelopeRequestBrokerError, EnvelopeRequestStatus, UserEnvelopeRequest, UserEnvelopeSession,
    ValidatedEnvelopeRequest, protected_router,
};

#[derive(Clone, Default)]
pub struct LocalEnvelopeRequestBroker {
    state: Arc<Mutex<LocalEnvelopeRequestState>>,
}

#[derive(Default)]
struct LocalEnvelopeRequestState {
    requests: HashMap<CanonicalUserId, Vec<LocalEnvelopeRequest>>,
}

#[derive(Clone)]
struct LocalEnvelopeRequest {
    idempotency_key: String,
    request: UserEnvelopeRequest,
}

impl LocalEnvelopeRequestBroker {
    pub fn new() -> Self {
        Self::default()
    }
}

fn demo_template() -> AvailableEnvelopeTemplate {
    let ceiling = Envelope {
        revision: 3,
        spec: EnvelopeSpec {
            llms: vec![ModelRef {
                provider: "openai".to_owned(),
                model: "gpt-5.4".to_owned(),
            }],
            tools: vec![ToolGrant {
                provider: "github".to_owned(),
                resource: "repository".to_owned(),
                action: "get_file_contents".to_owned(),
            }],
            budget: Budget {
                monthly_limit: "250.00".to_owned(),
                currency: "USD".to_owned(),
            },
            ttl: Duration("720h".to_owned()),
        },
    };
    AvailableEnvelopeTemplate {
        id: "engineer".to_owned(),
        display_name: "Engineer".to_owned(),
        revision: 3,
        auto_provision_threshold: Envelope {
            revision: 3,
            spec: EnvelopeSpec {
                budget: Budget {
                    monthly_limit: "100.00".to_owned(),
                    currency: "USD".to_owned(),
                },
                ttl: Duration("168h".to_owned()),
                ..ceiling.spec.clone()
            },
        },
        ceiling,
        // This describes the credential-free local broker's explicit readiness fixture; it
        // neither stores nor mints a provider credential.
        github_connection: ConnectionReadiness::Connected,
    }
}

impl EnvelopeRequestBroker<BrowserSessionBinding> for LocalEnvelopeRequestBroker {
    fn templates<'a>(
        &'a self,
        _session: &'a UserEnvelopeSession<BrowserSessionBinding>,
    ) -> BoxFuture<'a, Result<Vec<AvailableEnvelopeTemplate>, EnvelopeRequestBrokerError>> {
        Box::pin(async { Ok(vec![demo_template()]) })
    }

    fn list<'a>(
        &'a self,
        session: &'a UserEnvelopeSession<BrowserSessionBinding>,
    ) -> BoxFuture<'a, Result<Vec<UserEnvelopeRequest>, EnvelopeRequestBrokerError>> {
        Box::pin(async move {
            let state = self
                .state
                .lock()
                .map_err(|_| EnvelopeRequestBrokerError::Unavailable)?;
            Ok(state
                .requests
                .get(&session.subject.canonical_user_id)
                .map(|requests| {
                    requests
                        .iter()
                        .map(|request| request.request.clone())
                        .collect()
                })
                .unwrap_or_default())
        })
    }

    fn get<'a>(
        &'a self,
        session: &'a UserEnvelopeSession<BrowserSessionBinding>,
        request_id: Uuid,
    ) -> BoxFuture<'a, Result<Option<UserEnvelopeRequest>, EnvelopeRequestBrokerError>> {
        Box::pin(async move {
            let state = self
                .state
                .lock()
                .map_err(|_| EnvelopeRequestBrokerError::Unavailable)?;
            Ok(state
                .requests
                .get(&session.subject.canonical_user_id)
                .and_then(|requests| {
                    requests
                        .iter()
                        .find(|request| request.request.id == request_id)
                })
                .map(|request| request.request.clone()))
        })
    }

    fn create<'a>(
        &'a self,
        session: &'a UserEnvelopeSession<BrowserSessionBinding>,
        request: ValidatedEnvelopeRequest<'a>,
    ) -> BoxFuture<'a, Result<UserEnvelopeRequest, EnvelopeRequestBrokerError>> {
        let subject = session.subject.canonical_user_id.clone();
        let template_id = request.template.id.clone();
        let template_revision = request.template.revision;
        let requested_envelope = request.requested_envelope.clone();
        let idempotency_key = request.idempotency_key.to_owned();
        let auto_provision = request.auto_provision;
        Box::pin(async move {
            let mut state = self
                .state
                .lock()
                .map_err(|_| EnvelopeRequestBrokerError::Unavailable)?;
            let requests = state.requests.entry(subject).or_default();
            if let Some(existing) = requests
                .iter()
                .find(|existing| existing.idempotency_key == idempotency_key)
            {
                if existing.request.template_id == template_id
                    && existing.request.template_revision == template_revision
                    && existing.request.requested_envelope == requested_envelope
                {
                    return Ok(existing.request.clone());
                }
                return Err(EnvelopeRequestBrokerError::Conflict);
            }
            let id = Uuid::new_v4();
            let digest = format!(
                "sha256:{:x}",
                Sha256::digest(
                    serde_json::to_vec(&requested_envelope)
                        .map_err(|_| EnvelopeRequestBrokerError::Unavailable)?,
                )
            );
            let (status, envelope_instance_id, envelope_digest, approval_id) = if auto_provision {
                (
                    EnvelopeRequestStatus::Provisioned,
                    Some(format!("env_{}", id.simple())),
                    Some(digest),
                    None,
                )
            } else {
                (
                    EnvelopeRequestStatus::Pending,
                    None,
                    None,
                    Some(Uuid::new_v4()),
                )
            };
            let record = UserEnvelopeRequest {
                id,
                template_id,
                template_revision,
                requested_envelope,
                status,
                approval_id,
                envelope_instance_id,
                envelope_digest,
                reason: None,
                created_at: "2026-08-17T00:00:00Z".to_owned(),
                status_at: "2026-08-17T00:00:00Z".to_owned(),
            };
            requests.push(LocalEnvelopeRequest {
                idempotency_key,
                request: record.clone(),
            });
            Ok(record)
        })
    }
}

/// Local hand-test router: real browser session middleware plus the local, credential-free
/// envelope broker. It is valid only on an explicit loopback origin.
pub fn router(origin: &str) -> Result<Router, String> {
    let auth = local_fake_browser_auth_service(origin, LocalFakeIdentity::User)?;
    let post_sign_in = protect_browser_routes(
        Router::new().route(
            "/admin/connections",
            get(|| async { Redirect::to("/app/envelopes") }),
        ),
        auth.clone(),
    );
    Ok(browser_auth_router(auth.clone())
        .merge(post_sign_in)
        .merge(protected_router(LocalEnvelopeRequestBroker::new(), auth)))
}
