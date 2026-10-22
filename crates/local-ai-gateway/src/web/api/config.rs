//! The configuration over REST: the effective values (webhook header values masked), the file as text for
//! the editor, a dry-run check, a save that applies what can be applied, and a reload from disk.

use axum::Json;
use axum::extract::State;
use gateway_core::config::{Config, Issue, IssueLevel, SAMPLE};
use serde::{Deserialize, Serialize};

use super::{ApiError, ApiResult, ok};
use crate::hot::ApplyReport;
use crate::web::AppState;

pub async fn api_config(State(st): State<AppState>) -> ApiResult<Config> {
    ok(masked(&st.config()))
}

/// Webhook header values are credentials; nothing else in the file is secret.
pub fn masked(config: &Config) -> Config {
    let mut copy = config.clone();
    for value in copy.alerts.webhook.headers.values_mut() {
        *value = "[REDACTED]".to_string();
    }
    copy
}

#[derive(Debug, Serialize)]
pub struct ConfigFile {
    pub path: String,
    pub exists: bool,
    /// The file as written; empty when there is none.
    pub text: String,
    /// The commented sample with every default, for starting a file.
    pub sample: &'static str,
}

/// The file itself, header values included: the editor saves it back whole, so masking would overwrite
/// them. The dashboard viewer is the operator who wrote the file.
pub async fn api_config_file(State(st): State<AppState>) -> ApiResult<ConfigFile> {
    let path = st.hot().config_path().to_path_buf();
    let exists = path.exists();
    let text = if exists {
        std::fs::read_to_string(&path)
            .map_err(|err| ApiError::internal(anyhow::anyhow!("read {}: {err}", path.display())))?
    } else {
        String::new()
    };
    ok(ConfigFile {
        path: path.display().to_string(),
        exists,
        text,
        sample: SAMPLE,
    })
}

#[derive(Debug, Deserialize)]
pub struct ConfigText {
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct IssueView {
    pub level: &'static str,
    pub message: String,
}

impl From<Issue> for IssueView {
    fn from(issue: Issue) -> Self {
        IssueView {
            level: match issue.level {
                IssueLevel::Error => "error",
                IssueLevel::Warning => "warning",
            },
            message: issue.message,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ConfigCheck {
    /// No errors (warnings may remain).
    pub ok: bool,
    pub issues: Vec<IssueView>,
    /// Present after a save that applied.
    pub saved: bool,
    pub report: Option<ApplyReport>,
}

fn check_text(st: &AppState, text: &str) -> ConfigCheck {
    match Config::parse(text) {
        Ok(config) => {
            let issues = st.hot().check(&config);
            ConfigCheck {
                ok: !Config::has_errors(&issues),
                issues: issues.into_iter().map(IssueView::from).collect(),
                saved: false,
                report: None,
            }
        }
        Err(err) => ConfigCheck {
            ok: false,
            issues: vec![IssueView {
                level: "error",
                message: format!("{err:#}"),
            }],
            saved: false,
            report: None,
        },
    }
}

pub async fn api_config_validate(
    State(st): State<AppState>,
    Json(body): Json<ConfigText>,
) -> ApiResult<ConfigCheck> {
    ok(check_text(&st, &body.text))
}

/// Saves when there are no errors; the response carries the issues either way.
pub async fn api_config_put(
    State(st): State<AppState>,
    Json(body): Json<ConfigText>,
) -> ApiResult<ConfigCheck> {
    let mut check = check_text(&st, &body.text);
    if !check.ok {
        return ok(check);
    }
    let report = st
        .hot()
        .save_and_apply(&body.text)
        .map_err(|err| ApiError::internal(anyhow::anyhow!("save config: {err:#}")))?;
    check.saved = true;
    check.report = Some(report);
    ok(check)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_headers_are_masked() {
        let cfg = Config::parse("[alerts.webhook]\nurl = \"https://x\"\nheaders = { Authorization = \"Bearer s3cret\" }\n").unwrap();
        let shown = masked(&cfg);
        assert_eq!(shown.alerts.webhook.headers["Authorization"], "[REDACTED]");
        assert_eq!(shown.alerts.webhook.url.as_deref(), Some("https://x"));
    }
}
