//! `GET /api/models?backend=`: the unified model list, loaded first, then cloud, then not loaded.

use axum::extract::{Query, State};
use gateway_core::types::{Backend, ModelInfo};
use serde::Deserialize;

use super::{ApiResult, ok};
use crate::web::AppState;

#[derive(Debug, Deserialize)]
pub struct ModelsQuery {
    pub backend: Option<Backend>,
}

pub async fn api_models(
    State(st): State<AppState>,
    Query(q): Query<ModelsQuery>,
) -> ApiResult<Vec<ModelInfo>> {
    let mut models: Vec<ModelInfo> = match q.backend {
        Some(backend) => st.backends().models(backend).await,
        None => {
            let mut all = Vec::new();
            for snapshot in st.backends().all().await {
                all.extend(snapshot.models);
            }
            all
        }
    };
    models.sort_by_key(|m| (m.state.sort_key(), m.backend, m.id.to_ascii_lowercase()));
    ok(models)
}
