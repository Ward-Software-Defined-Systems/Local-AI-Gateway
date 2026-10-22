//! LM Studio's version. The server has no version endpoint, so on macOS it is read from the app bundle's
//! `Info.plist` and on Linux, best effort, from `lms version` when the CLI is configured. Either lookup is a
//! short-lived child process with a timeout; a failure just leaves the version unknown.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tracing::debug;

const TIMEOUT: Duration = Duration::from_secs(5);

/// macOS: `defaults read "<app>/Contents/Info.plist" CFBundleShortVersionString`.
pub async fn lmstudio_version(app_path: Option<&Path>, lms_bin: Option<&Path>) -> Option<String> {
    if cfg!(target_os = "macos") {
        let app = app_path
            .map(Path::to_path_buf)
            .unwrap_or_else(|| Path::new("/Applications/LM Studio.app").to_path_buf());
        let plist = app.join("Contents").join("Info.plist");
        if plist.is_file()
            && let Some(out) = run(Command::new("defaults")
                .arg("read")
                .arg(&plist)
                .arg("CFBundleShortVersionString"))
            .await
        {
            return Some(out);
        }
    }
    if let Some(lms) = lms_bin
        && let Some(out) = run(Command::new(lms).arg("version")).await
    {
        return parse_lms_version(&out);
    }
    None
}

/// `lms version` prints either a plain version or a line with a commit hash; take the first token that
/// looks like `x.y.z`.
pub fn parse_lms_version(output: &str) -> Option<String> {
    output
        .split(|c: char| c.is_whitespace() || c == ',')
        .map(|t| t.trim_start_matches('v'))
        .find(|t| {
            let mut parts = t.split('.');
            parts.clone().count() >= 2
                && parts.all(|p| {
                    !p.is_empty()
                        && p.chars()
                            .all(|c| c.is_ascii_digit() || c == '+' || c == '-')
                })
        })
        .map(str::to_string)
}

async fn run(cmd: &mut Command) -> Option<String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(TIMEOUT, cmd.output()).await {
        Ok(Ok(output)) if output.status.success() => output,
        Ok(Ok(output)) => {
            debug!(status = %output.status, "version lookup command failed");
            return None;
        }
        Ok(Err(err)) => {
            debug!(error = %err, "version lookup command could not run");
            return None;
        }
        Err(_) => {
            debug!("version lookup command timed out");
            return None;
        }
    };
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lms_version_output_variants_parse() {
        assert_eq!(parse_lms_version("0.0.52").as_deref(), Some("0.0.52"));
        assert_eq!(
            parse_lms_version("lms version v0.0.52 (abc123)").as_deref(),
            Some("0.0.52")
        );
        assert_eq!(parse_lms_version("0.4.25+1\n").as_deref(), Some("0.4.25+1"));
        assert_eq!(parse_lms_version("abc123def"), None);
        assert_eq!(parse_lms_version(""), None);
    }

    #[tokio::test]
    async fn a_missing_app_and_cli_give_no_version() {
        let dir = std::env::temp_dir().join("laig-no-such-app.app");
        assert_eq!(
            lmstudio_version(Some(&dir), Some(Path::new("/nonexistent/lms"))).await,
            None
        );
    }
}
