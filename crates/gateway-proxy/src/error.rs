//! The gateway's own error responses: what the client gets when the backend cannot be reached or the
//! request cannot be handled. Each maps to a status, a stable code and a message; `http::error_body` shapes
//! it for the client's API family.

use axum::http::StatusCode;
use gateway_core::types::{ApiFormat, Backend};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProxyError {
    #[error("upstream unreachable")]
    UpstreamUnreachable,
    #[error("request body too large")]
    BodyTooLarge,
    #[error("could not read request body")]
    BodyUnreadable,
    #[error("could not build response")]
    ResponseBuild,
    #[error("unsupported method")]
    UnsupportedMethod,
    #[error("upgrade failed")]
    UpgradeFailed,
    #[error("request blocked by gateway DLP policy (rule {rule_id})")]
    Blocked { rule_id: String },
    /// A cloud provider does not serve the request's API format; nothing was sent to it.
    #[error("{format} requests are not served by {}; use a format it speaks or a model another backend serves", backend.display_name())]
    FormatMismatch { format: ApiFormat, backend: Backend },
    /// The request carries a header that belongs to another cloud provider's API, so forwarding it would
    /// hand that provider's credential to the wrong host.
    #[error("the request carries {header}, which belongs to another provider's API, but is addressed to {}", backend.display_name())]
    CredentialMismatch {
        header: &'static str,
        backend: Backend,
    },
}

impl ProxyError {
    pub fn status(&self) -> StatusCode {
        match self {
            ProxyError::UpstreamUnreachable | ProxyError::UpgradeFailed => StatusCode::BAD_GATEWAY,
            ProxyError::BodyTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            ProxyError::BodyUnreadable
            | ProxyError::FormatMismatch { .. }
            | ProxyError::CredentialMismatch { .. } => StatusCode::BAD_REQUEST,
            ProxyError::ResponseBuild => StatusCode::INTERNAL_SERVER_ERROR,
            ProxyError::UnsupportedMethod => StatusCode::METHOD_NOT_ALLOWED,
            ProxyError::Blocked { .. } => StatusCode::FORBIDDEN,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            ProxyError::UpstreamUnreachable => "upstream_unreachable",
            ProxyError::BodyTooLarge => "request_too_large",
            ProxyError::BodyUnreadable => "request_unreadable",
            ProxyError::ResponseBuild => "response_build_failed",
            ProxyError::UnsupportedMethod => "unsupported_method",
            ProxyError::UpgradeFailed => "upgrade_failed",
            ProxyError::Blocked { .. } => "dlp_blocked",
            ProxyError::FormatMismatch { .. } => "format_not_served",
            ProxyError::CredentialMismatch { .. } => "credential_mismatch",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_map_to_statuses_and_codes() {
        assert_eq!(
            ProxyError::BodyTooLarge.status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(ProxyError::BodyUnreadable.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            ProxyError::UpstreamUnreachable.status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            ProxyError::Blocked {
                rule_id: "r".into()
            }
            .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            ProxyError::Blocked {
                rule_id: "r".into()
            }
            .code(),
            "dlp_blocked"
        );
        assert_eq!(
            ProxyError::Blocked {
                rule_id: "aws".into()
            }
            .to_string(),
            "request blocked by gateway DLP policy (rule aws)"
        );
        let mismatch = ProxyError::FormatMismatch {
            format: ApiFormat::OpenAiEmbeddings,
            backend: Backend::Anthropic,
        };
        assert_eq!(mismatch.status(), StatusCode::BAD_REQUEST);
        assert_eq!(mismatch.code(), "format_not_served");
        assert!(
            mismatch
                .to_string()
                .starts_with("openai_embeddings requests are not served by Anthropic"),
            "{mismatch}"
        );
        let credential = ProxyError::CredentialMismatch {
            header: "x-goog-api-key",
            backend: Backend::Anthropic,
        };
        assert_eq!(credential.status(), StatusCode::BAD_REQUEST);
        assert_eq!(credential.code(), "credential_mismatch");
        assert!(
            credential.to_string().contains("x-goog-api-key"),
            "{credential}"
        );
    }
}
