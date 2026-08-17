//! Credential-free, loopback-only provider collaborator for localhost acceptance.

use std::collections::HashMap;
use std::hash::Hash;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use steward_types::CanonicalUserId;

use crate::BoxFuture;
use crate::connections::{
    AuthorizationUrl, ConnectionBrokerError, ConnectionContinuation, ConnectionSession,
    ProviderConnectionBroker, ProviderConnectionStatus,
};

const LOCAL_REQUIRED_SCOPES: [&str; 5] = [
    "repo",
    "read:org",
    "workflow",
    "notifications",
    "user:email",
];

struct LocalFlow<B> {
    user_id: CanonicalUserId,
    binding: B,
}

struct LocalAccount {
    email: String,
}

struct LocalConnectionsState<B> {
    flows: HashMap<String, LocalFlow<B>>,
    accounts: HashMap<CanonicalUserId, LocalAccount>,
}

impl<B> Default for LocalConnectionsState<B> {
    fn default() -> Self {
        Self {
            flows: HashMap::new(),
            accounts: HashMap::new(),
        }
    }
}

pub struct LocalConnectionsBroker<B> {
    bind: SocketAddr,
    next_flow: Arc<AtomicU64>,
    state: Arc<Mutex<LocalConnectionsState<B>>>,
}

impl<B> Clone for LocalConnectionsBroker<B> {
    fn clone(&self) -> Self {
        Self {
            bind: self.bind,
            next_flow: Arc::clone(&self.next_flow),
            state: Arc::clone(&self.state),
        }
    }
}

impl<B> LocalConnectionsBroker<B> {
    pub fn new(bind: SocketAddr) -> Result<Self, String> {
        if !bind.ip().is_loopback() {
            return Err("localhost Connections broker bind must be loopback".to_owned());
        }
        Ok(Self {
            bind,
            next_flow: Arc::new(AtomicU64::new(1)),
            state: Arc::new(Mutex::new(LocalConnectionsState::default())),
        })
    }
}

impl<B> ProviderConnectionBroker<B> for LocalConnectionsBroker<B>
where
    B: Clone + Eq + Hash + Send + Sync + 'static,
{
    fn status<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
    ) -> BoxFuture<'a, Result<ProviderConnectionStatus, ConnectionBrokerError>> {
        Box::pin(async move {
            let state = self
                .state
                .lock()
                .map_err(|_| ConnectionBrokerError::Unavailable)?;
            let account = state.accounts.get(&session.subject.canonical_user_id);
            let connected = account.is_some();
            let scopes_required = LOCAL_REQUIRED_SCOPES.map(str::to_owned).to_vec();
            Ok(ProviderConnectionStatus {
                phase: if connected {
                    crate::connections::ConnectionPhase::Connected
                } else {
                    crate::connections::ConnectionPhase::Disconnected
                },
                account_email: account.map(|account| account.email.clone()),
                scopes_granted: if connected {
                    scopes_required.clone()
                } else {
                    Vec::new()
                },
                scopes_missing: if connected {
                    Vec::new()
                } else {
                    scopes_required.clone()
                },
                scopes_required,
                expires_at: None,
            })
        })
    }

    fn start<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
    ) -> BoxFuture<'a, Result<AuthorizationUrl, ConnectionBrokerError>> {
        Box::pin(async move {
            let sequence = self.next_flow.fetch_add(1, Ordering::Relaxed);
            let continuation = format!("local-flow-{sequence}");
            self.state
                .lock()
                .map_err(|_| ConnectionBrokerError::Unavailable)?
                .flows
                .insert(
                    continuation.clone(),
                    LocalFlow {
                        user_id: session.subject.canonical_user_id.clone(),
                        binding: session.binding.clone(),
                    },
                );
            AuthorizationUrl::new_loopback(format!(
                "http://{}/admin/connections/github/callback?continuation={continuation}",
                self.bind
            ))
            .map_err(|_| ConnectionBrokerError::Unavailable)
        })
    }

    fn complete<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
        continuation: &'a ConnectionContinuation,
    ) -> BoxFuture<'a, Result<(), ConnectionBrokerError>> {
        Box::pin(async move {
            let mut state = self
                .state
                .lock()
                .map_err(|_| ConnectionBrokerError::Unavailable)?;
            let Some(flow) = state.flows.get(continuation.as_str()) else {
                return Err(ConnectionBrokerError::InvalidOrExpiredContinuation);
            };
            if flow.user_id != session.subject.canonical_user_id || flow.binding != session.binding
            {
                return Err(ConnectionBrokerError::SessionMismatch);
            }
            state.flows.remove(continuation.as_str());
            state.accounts.insert(
                session.subject.canonical_user_id.clone(),
                LocalAccount {
                    email: session.subject.display_email.clone(),
                },
            );
            Ok(())
        })
    }

    fn disconnect<'a>(
        &'a self,
        session: &'a ConnectionSession<B>,
    ) -> BoxFuture<'a, Result<(), ConnectionBrokerError>> {
        Box::pin(async move {
            self.state
                .lock()
                .map_err(|_| ConnectionBrokerError::Unavailable)?
                .accounts
                .remove(&session.subject.canonical_user_id);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use steward_types::CanonicalUserId;

    use super::*;
    use crate::connections::{ConnectionPhase, ConnectionSubject};

    #[derive(Clone, Eq, Hash, PartialEq)]
    struct TestBinding(&'static str);

    fn session(binding: &'static str) -> Result<ConnectionSession<TestBinding>, String> {
        Ok(ConnectionSession {
            subject: ConnectionSubject {
                canonical_user_id: CanonicalUserId::parse("usr_0123456789abcdef0123456789abcdef")?,
                display_email: "alice@example.com".to_owned(),
            },
            binding: TestBinding(binding),
        })
    }

    #[test]
    fn local_broker_rejects_every_non_loopback_bind() {
        assert!(
            LocalConnectionsBroker::<TestBinding>::new(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                3000,
            ))
            .is_err()
        );
    }

    #[tokio::test]
    async fn local_broker_runs_one_time_same_session_connection_without_any_credential()
    -> Result<(), String> {
        let broker =
            LocalConnectionsBroker::new(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 43123))?;
        let disconnected = broker
            .status(&session("session-a")?)
            .await
            .map_err(|error| format!("read initial local status: {error:?}"))?;
        assert_eq!(disconnected.phase, ConnectionPhase::Disconnected);

        let authorization = broker
            .start(&session("session-a")?)
            .await
            .map_err(|error| format!("start local connection: {error:?}"))?;
        assert_eq!(
            authorization.as_str(),
            "http://127.0.0.1:43123/admin/connections/github/callback?continuation=local-flow-1"
        );
        let continuation =
            ConnectionContinuation::new("local-flow-1".to_owned()).map_err(str::to_owned)?;
        assert_eq!(
            broker.complete(&session("session-b")?, &continuation).await,
            Err(ConnectionBrokerError::SessionMismatch)
        );
        broker
            .complete(&session("session-a")?, &continuation)
            .await
            .map_err(|error| format!("complete local connection: {error:?}"))?;
        assert_eq!(
            broker.complete(&session("session-a")?, &continuation).await,
            Err(ConnectionBrokerError::InvalidOrExpiredContinuation)
        );

        let connected = broker
            .status(&session("session-a")?)
            .await
            .map_err(|error| format!("read connected local status: {error:?}"))?;
        assert_eq!(connected.phase, ConnectionPhase::Connected);
        assert_eq!(
            connected.account_email.as_deref(),
            Some("alice@example.com")
        );
        assert_eq!(connected.scopes_required, connected.scopes_granted);
        assert!(connected.scopes_missing.is_empty());
        assert_eq!(connected.expires_at, None);

        broker
            .disconnect(&session("session-a")?)
            .await
            .map_err(|error| format!("disconnect local connection: {error:?}"))?;
        broker
            .disconnect(&session("session-a")?)
            .await
            .map_err(|error| format!("repeat local disconnect: {error:?}"))?;
        assert_eq!(
            broker
                .status(&session("session-a")?)
                .await
                .map_err(|error| format!("read disconnected local status: {error:?}"))?
                .phase,
            ConnectionPhase::Disconnected
        );
        Ok(())
    }
}
