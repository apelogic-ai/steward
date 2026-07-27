//! SPIRE-backed workload-identity validation.

use spiffe::WorkloadApiClient;
use steward_ports::{
    PortError, SvidAssertion, SvidValidationError, ValidatedWorkload, WorkloadIdentity,
};

pub const IMPLEMENTED_PORTS: [&str; 1] = ["WorkloadIdentity"];

#[derive(Clone)]
pub struct SpireSvidValidator {
    client: WorkloadApiClient,
}

impl SpireSvidValidator {
    pub async fn connect_env() -> Result<Self, PortError> {
        WorkloadApiClient::connect_env()
            .await
            .map(|client| Self { client })
            .map_err(|_| PortError::Failed {
                reason: "SPIRE Workload API is unavailable".to_owned(),
            })
    }

    pub const fn new(client: WorkloadApiClient) -> Self {
        Self { client }
    }
}

fn classify_workload_api_error(
    error: spiffe::workload_api::WorkloadApiError,
) -> SvidValidationError {
    match error {
        spiffe::workload_api::WorkloadApiError::PermissionDenied(_)
        | spiffe::workload_api::WorkloadApiError::NoIdentityIssued
        | spiffe::workload_api::WorkloadApiError::JwtSvid(_) => SvidValidationError::Rejected,
        spiffe::workload_api::WorkloadApiError::Transport(
            spiffe::transport::TransportError::Status(status),
        ) if matches!(
            status.code(),
            tonic::Code::InvalidArgument
                | tonic::Code::Unauthenticated
                | tonic::Code::PermissionDenied
        ) =>
        {
            SvidValidationError::Rejected
        }
        _ => SvidValidationError::Unavailable,
    }
}

impl WorkloadIdentity for SpireSvidValidator {
    fn validate(
        &self,
        audience: &str,
        assertion: &SvidAssertion,
    ) -> impl Future<Output = Result<ValidatedWorkload, SvidValidationError>> + Send {
        let client = self.client.clone();
        let audience = audience.to_owned();
        let assertion = assertion.expose_secret().to_owned();
        async move {
            let svid = client
                .validate_jwt_token(&audience, &assertion)
                .await
                .map_err(classify_workload_api_error)?;
            Ok(ValidatedWorkload {
                spiffe_id: svid.spiffe_id().to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use spiffe::{transport::TransportError, workload_api::WorkloadApiError};
    use tonic::Status;

    use super::{SvidValidationError, classify_workload_api_error};

    #[test]
    fn spire_rejects_invalid_assertions_without_masking_outages() {
        let invalid_assertion = WorkloadApiError::Transport(TransportError::Status(
            Status::invalid_argument("invalid workload assertion"),
        ));
        assert_eq!(
            classify_workload_api_error(invalid_assertion),
            SvidValidationError::Rejected
        );

        let unavailable = WorkloadApiError::Transport(TransportError::Status(Status::unavailable(
            "workload API unavailable",
        )));
        assert_eq!(
            classify_workload_api_error(unavailable),
            SvidValidationError::Unavailable
        );
    }
}
