//! Deterministic TODO and GitHub observation. No models and no repository mutations.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use color_eyre::eyre::{Context, Result, bail, ensure, eyre};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{io::AsyncReadExt, process::Command, time::timeout};

use crate::{paths::digest, protocol::AgentId};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct TodoWatch {
    pub path: PathBuf,
    pub driver: AgentId,
    pub total: usize,
    pub remaining: usize,
    pub next: String,
    pub signature: String,
    pub nudges: u32,
    pub next_at: u64,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct TodoSnapshot {
    pub total: usize,
    pub remaining: usize,
    pub next: String,
    pub signature: String,
}

pub(crate) fn read_todo(path: &Path) -> Result<TodoSnapshot> {
    // Only an explicitly attached file; no recursive discovery or link traversal.
    crate::paths::check_regular(path)?;
    let file = std::fs::File::open(path)?;
    let mut text = String::new();
    std::io::Read::read_to_string(&mut std::io::Read::take(file, 1024 * 1024 + 1), &mut text)?;
    ensure!(text.len() <= 1024 * 1024, "TODO exceeds 1 MiB");
    Ok(parse_todo(&text))
}

fn parse_todo(text: &str) -> TodoSnapshot {
    let mut total = 0;
    let mut open = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        let line = line.trim_start();
        if line.starts_with("```") || line.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let Some(rest) = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .or_else(|| line.strip_prefix("+ "))
        else {
            continue;
        };
        if let Some(body) = rest.strip_prefix("[ ] ") {
            total += 1;
            open.push(body.trim().to_owned());
        } else if rest.starts_with("[x] ") || rest.starts_with("[X] ") {
            total += 1;
        }
    }
    let next = open
        .first()
        .map(|line| line.chars().take(400).collect())
        .unwrap_or_default();
    TodoSnapshot {
        total,
        remaining: open.len(),
        next,
        signature: digest(format!("{total}:{open:?}")),
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PrBaseline {
    pub feedback: BTreeMap<String, String>,
    pub failures: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PrWatch {
    pub url: String,
    pub cwd: PathBuf,
    pub driver: AgentId,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub baseline: Option<PrBaseline>,
    pub next_at: u64,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct PrSnapshot {
    pub branch: String,
    pub head: String,
    pub local_head: String,
    pub open: bool,
    pub feedback: BTreeMap<String, (String, String)>,
    pub failures: BTreeMap<String, String>,
}

/// Canonical github.com URLs only in v1, avoiding arbitrary API hosts/arguments.
pub(crate) fn pr_target(url: &str) -> Result<(String, u64)> {
    let tail = url
        .strip_prefix("https://github.com/")
        .ok_or_else(|| eyre!("expected https://github.com/OWNER/REPO/pull/NUMBER"))?;
    let parts: Vec<_> = tail.trim_end_matches('/').split('/').collect();
    ensure!(
        parts.len() == 4 && parts[2] == "pull",
        "invalid GitHub PR URL"
    );
    for name in &parts[..2] {
        ensure!(
            !name.is_empty()
                && *name != "."
                && *name != ".."
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
            "invalid GitHub repository"
        );
    }
    let number: u64 = parts[3].parse().wrap_err("invalid PR number")?;
    ensure!(number > 0, "invalid PR number");
    Ok((format!("{}/{}", parts[0], parts[1]), number))
}

async fn run(program: &str, args: &[&str], cwd: &Path) -> Result<String> {
    timeout(Duration::from_secs(30), async {
        let mut child = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_PAGER", "cat")
            .env("GH_DEBUG", "")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PAGER", "cat")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .wrap_err_with(|| {
                format!("starting {program}; ensure it is installed and authenticated")
            })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| eyre!("missing subprocess stdout"))?;
        let mut bytes = Vec::new();
        stdout
            .take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() > 16 * 1024 * 1024 {
            child.kill().await?;
            bail!("{program} output exceeded 16 MiB");
        }
        ensure!(
            child.wait().await?.success(),
            "{program} failed; check authentication and the selected checkout/PR"
        );
        String::from_utf8(bytes).wrap_err("subprocess output was not UTF-8")
    })
    .await
    .wrap_err_with(|| format!("{program} timed out"))?
}

pub(crate) async fn collect_pr(watch: &PrWatch) -> Result<PrSnapshot> {
    let (repo, number) = pr_target(&watch.url)?;
    let branch = run("git", &["rev-parse", "--abbrev-ref", "HEAD"], &watch.cwd)
        .await?
        .trim()
        .to_owned();
    let local_head = run("git", &["rev-parse", "HEAD"], &watch.cwd)
        .await?
        .trim()
        .to_owned();
    ensure!(
        branch != "HEAD",
        "PR watch requires a branch, not detached HEAD"
    );
    if let Some(expected) = &watch.branch {
        ensure!(
            expected == &branch,
            "local branch changed; reconfigure PR watch"
        );
    }
    let raw: Value = serde_json::from_str(
        &run(
            "gh",
            &[
                "pr",
                "view",
                &watch.url,
                "--json",
                "url,state,headRefName,headRefOid,comments,reviews,statusCheckRollup",
            ],
            &watch.cwd,
        )
        .await?,
    )?;
    let endpoint = format!("repos/{repo}/pulls/{number}/comments?per_page=100");
    let pages: Value = serde_json::from_str(
        &run(
            "gh",
            &["api", &endpoint, "--paginate", "--slurp"],
            &watch.cwd,
        )
        .await?,
    )?;
    let mut snapshot = normalize_pr(&watch.url, &raw, &pages)?;
    ensure!(
        snapshot.branch == branch,
        "local branch does not match the PR head branch"
    );
    let fresh: Value = serde_json::from_str(
        &run(
            "gh",
            &[
                "pr",
                "view",
                &watch.url,
                "--json",
                "headRefOid,headRefName,state",
            ],
            &watch.cwd,
        )
        .await?,
    )?;
    ensure!(
        fresh["headRefOid"] == raw["headRefOid"]
            && fresh["headRefName"] == raw["headRefName"]
            && fresh["state"] == raw["state"],
        "PR changed during polling; baseline was not advanced"
    );
    let final_branch = run("git", &["rev-parse", "--abbrev-ref", "HEAD"], &watch.cwd).await?;
    let final_head = run("git", &["rev-parse", "HEAD"], &watch.cwd).await?;
    ensure!(
        final_branch.trim() == branch && final_head.trim() == local_head,
        "local checkout changed during polling; reconfigure PR watch"
    );
    snapshot.local_head = local_head;
    Ok(snapshot)
}

fn array<'a>(value: &'a Value, label: &str) -> Result<&'a Vec<Value>> {
    value
        .as_array()
        .ok_or_else(|| eyre!("invalid GitHub {label}; refusing a partial baseline"))
}
fn text<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value[field]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| eyre!("missing GitHub field {field}"))
}

pub(crate) fn normalize_pr(url: &str, raw: &Value, pages: &Value) -> Result<PrSnapshot> {
    ensure!(
        text(raw, "url")?.trim_end_matches('/') == url.trim_end_matches('/'),
        "GitHub returned another PR"
    );
    let mut feedback = BTreeMap::new();
    let inline: Vec<Value> = array(pages, "inline pages")?
        .iter()
        .map(|p| array(p, "inline page"))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .cloned()
        .collect();
    for (kind, rows) in [
        ("comment", array(&raw["comments"], "comments")?),
        ("review", array(&raw["reviews"], "reviews")?),
        ("inline", &inline),
    ] {
        for row in rows {
            let body = row["body"].as_str().unwrap_or_default();
            if row["state"] == "PENDING"
                || (body.trim().is_empty() && row["state"] != "CHANGES_REQUESTED")
            {
                continue;
            }
            ensure!(
                row["id"].is_string() || row["id"].is_number(),
                "feedback has no stable ID"
            );
            let content = json!([
                body,
                row["state"],
                row["path"],
                row["line"],
                row["author"],
                row["user"]
            ]);
            let link = row["url"]
                .as_str()
                .or_else(|| row["html_url"].as_str())
                .unwrap_or(url);
            let summary = format!(
                "{kind} {}: {} — {link}",
                row["state"].as_str().unwrap_or_default(),
                body.chars().take(350).collect::<String>()
            );
            feedback.insert(
                format!("{kind}:{}", row["id"]),
                (digest(content.to_string()), summary),
            );
        }
    }
    let mut failures = BTreeMap::new();
    let empty = Vec::new();
    let checks = if raw["statusCheckRollup"].is_null() {
        &empty
    } else {
        array(&raw["statusCheckRollup"], "checks")?
    };
    for row in checks {
        let (failed, name, link) = match text(row, "__typename")? {
            "CheckRun" => (
                row["status"] == "COMPLETED"
                    && ["FAILURE", "TIMED_OUT", "STARTUP_FAILURE", "ACTION_REQUIRED"]
                        .contains(&row["conclusion"].as_str().unwrap_or_default()),
                row["name"].as_str().unwrap_or("check"),
                row["detailsUrl"].as_str().unwrap_or(url),
            ),
            "StatusContext" => (
                ["FAILURE", "ERROR"].contains(&row["state"].as_str().unwrap_or_default()),
                row["context"].as_str().unwrap_or("status"),
                row["targetUrl"].as_str().unwrap_or(url),
            ),
            other => bail!("unknown GitHub check type {other}"),
        };
        if failed {
            failures.insert(
                digest(json!([raw["headRefOid"], row]).to_string()),
                format!("Failed check: {name} — {link}"),
            );
        }
    }
    Ok(PrSnapshot {
        branch: text(raw, "headRefName")?.to_owned(),
        head: text(raw, "headRefOid")?.to_owned(),
        local_head: String::new(),
        open: text(raw, "state")? == "OPEN",
        feedback,
        failures,
    })
}

impl PrSnapshot {
    pub fn initial_baseline(&self) -> PrBaseline {
        PrBaseline {
            feedback: self
                .feedback
                .iter()
                .map(|(id, (hash, _))| (id.clone(), hash.clone()))
                .collect(),
            failures: Vec::new(),
        }
    }

    pub fn delta(&self, baseline: &PrBaseline) -> (Vec<String>, PrBaseline) {
        let mut lines = Vec::new();
        let mut acknowledged = baseline.clone();
        for (id, (hash, summary)) in &self.feedback {
            if baseline.feedback.get(id) != Some(hash) {
                lines.push(summary.clone());
                acknowledged.feedback.insert(id.clone(), hash.clone());
            }
        }
        for (hash, summary) in &self.failures {
            if !baseline.failures.contains(hash) {
                lines.push(summary.clone());
            }
        }
        acknowledged.failures = self.failures.keys().cloned().collect();
        (lines, acknowledged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const URL: &str = "https://github.com/example/repo/pull/1";
    fn fixture() -> Value {
        json!({"url":URL,"state":"OPEN","headRefName":"feature","headRefOid":"abc","comments":[{"id":"c1","body":"old"}],"reviews":[{"id":"r1","body":"","state":"CHANGES_REQUESTED"}],"statusCheckRollup":[{"__typename":"CheckRun","status":"COMPLETED","conclusion":"FAILURE","name":"test","startedAt":"1"}]})
    }

    #[test]
    fn todo_counts_nested_criteria_but_not_code_examples() {
        let value =
            parse_todo("- [ ] parent\n  - [X] checked\n  - [ ] child\n```\n- [ ] example\n```\n");
        assert_eq!(
            (value.total, value.remaining, value.next.as_str()),
            (3, 2, "parent")
        );
    }

    #[test]
    fn first_poll_baselines_feedback_but_not_failures() {
        let snapshot = normalize_pr(
            URL,
            &fixture(),
            &json!([[{"id": 1, "body":"inline"}], [{"id":2,"body":"reply"}]]),
        )
        .unwrap();
        assert_eq!(snapshot.feedback.len(), 4);
        let (delta, baseline) = snapshot.delta(&snapshot.initial_baseline());
        assert_eq!(delta.len(), 1);
        assert!(snapshot.delta(&baseline).0.is_empty());
        let mut raw = fixture();
        raw["comments"][0]["body"] = json!("edited");
        let edited = normalize_pr(URL, &raw, &json!([[]])).unwrap();
        assert_eq!(edited.delta(&baseline).0.len(), 1);
        raw["headRefOid"] = json!("def");
        assert_eq!(
            normalize_pr(URL, &raw, &json!([[]]))
                .unwrap()
                .delta(&baseline)
                .0
                .len(),
            2
        );
    }

    #[test]
    fn refuses_partial_data_and_untrusted_urls() {
        assert!(normalize_pr(URL, &fixture(), &json!([{}])).is_err());
        for url in [
            "http://github.com/a/b/pull/1",
            "https://github.com/a/b/pull/1?x=1",
            "https://elsewhere/a/b/pull/1",
        ] {
            assert!(pr_target(url).is_err());
        }
    }
}
