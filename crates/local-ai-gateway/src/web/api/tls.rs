//! `GET /api/tls/ca`: the gateway's CA certificate as a PEM download, for the machines and applications
//! that must trust the HTTPS listeners. The certificate is public; the key never leaves the data directory.

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::ApiError;
use crate::web::AppState;

pub async fn api_ca_cert(State(st): State<AppState>) -> Result<Response, ApiError> {
    let path = st
        .hot()
        .paths()
        .tls_dir()
        .join(gateway_core::tls::CA_CERT_FILE);
    let pem = tokio::fs::read(&path).await.map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            ApiError::not_found("the CA certificate has not been created yet")
        } else {
            ApiError::internal(anyhow::Error::new(err).context("read the CA certificate"))
        }
    })?;
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/x-pem-file"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"local-ai-gateway-ca.pem\"",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        pem,
    )
        .into_response())
}
