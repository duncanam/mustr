//! Deterministic TODO and GitHub observation. No models and no repository mutations.

use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
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
    #[serde(default)]
    pub url: Option<String>,
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
    pub url: String,
    pub cwd: PathBuf,
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

type RunFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

trait PrCommandRunner: Send {
    fn run<'a>(&'a mut self, program: &'a str, args: &'a [&'a str], cwd: &'a Path)
    -> RunFuture<'a>;
}

struct SystemPrCommandRunner;

impl PrCommandRunner for SystemPrCommandRunner {
    fn run<'a>(
        &'a mut self,
        program: &'a str,
        args: &'a [&'a str],
        cwd: &'a Path,
    ) -> RunFuture<'a> {
        Box::pin(run_command(program, args, cwd))
    }
}

async fn run_command(program: &str, args: &[&str], cwd: &Path) -> Result<String> {
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
    collect_pr_with(watch, &mut SystemPrCommandRunner).await
}

async fn collect_pr_with<R: PrCommandRunner>(
    watch: &PrWatch,
    runner: &mut R,
) -> Result<PrSnapshot> {
    let initial = checkout_identity(runner, &watch.cwd).await?;
    let (url, cwd, discovering) = match &watch.url {
        Some(url) => {
            pr_target(url)?;
            (
                url.trim_end_matches('/').to_owned(),
                watch.cwd.clone(),
                None,
            )
        }
        None => {
            let url = discover_pr(runner, &watch.cwd, &initial.branch).await?;
            (url, initial.root.clone(), Some(initial.clone()))
        }
    };
    let mut snapshot = collect_pr_url(runner, &url, &cwd, watch.branch.as_deref()).await?;
    if let Some(initial) = discovering {
        ensure!(snapshot.open, "discovered PR is no longer open");
        ensure!(
            snapshot.cwd == initial.root
                && snapshot.branch == initial.branch
                && snapshot.local_head == initial.head,
            "local checkout changed during PR discovery; retry from the current checkout"
        );
        snapshot.cwd = initial.root;
    }
    Ok(snapshot)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CheckoutIdentity {
    root: PathBuf,
    branch: String,
    head: String,
}

async fn checkout_identity<R: PrCommandRunner>(
    runner: &mut R,
    cwd: &Path,
) -> Result<CheckoutIdentity> {
    ensure!(
        cwd.is_absolute() && cwd.is_dir(),
        "PR checkout is unavailable"
    );
    let root = PathBuf::from(
        runner
            .run("git", &["rev-parse", "--show-toplevel"], cwd)
            .await?
            .trim(),
    );
    ensure!(
        root.is_absolute() && root.is_dir(),
        "git returned an invalid checkout root"
    );
    let branch = runner
        .run("git", &["rev-parse", "--abbrev-ref", "HEAD"], cwd)
        .await?
        .trim()
        .to_owned();
    ensure!(
        !branch.is_empty() && branch != "HEAD",
        "PR watch requires a branch, not detached HEAD"
    );
    let head = runner
        .run("git", &["rev-parse", "HEAD"], cwd)
        .await?
        .trim()
        .to_owned();
    ensure!(!head.is_empty(), "git returned an empty local HEAD");
    Ok(CheckoutIdentity { root, branch, head })
}

async fn discover_pr<R: PrCommandRunner>(
    runner: &mut R,
    cwd: &Path,
    branch: &str,
) -> Result<String> {
    let raw: Value = serde_json::from_str(
        &runner
            .run(
                "gh",
                &["pr", "view", "--json", "url,headRefName,state"],
                cwd,
            )
            .await?,
    )?;
    let url = text(&raw, "url")?.to_owned();
    pr_target(&url)?;
    ensure!(
        text(&raw, "state")? == "OPEN",
        "current branch PR is not open"
    );
    ensure!(
        text(&raw, "headRefName")? == branch,
        "local branch does not match the discovered PR head branch"
    );
    Ok(url.trim_end_matches('/').to_owned())
}

async fn collect_pr_url<R: PrCommandRunner>(
    runner: &mut R,
    url: &str,
    cwd: &Path,
    expected_branch: Option<&str>,
) -> Result<PrSnapshot> {
    let (repo, number) = pr_target(url)?;
    let local = checkout_identity(runner, cwd).await?;
    if let Some(expected) = expected_branch {
        ensure!(
            expected == local.branch,
            "local branch changed; reconfigure PR watch"
        );
    }
    let raw: Value = serde_json::from_str(
        &runner
            .run(
                "gh",
                &[
                    "pr",
                    "view",
                    url,
                    "--json",
                    "url,state,headRefName,headRefOid,comments,reviews,statusCheckRollup",
                ],
                cwd,
            )
            .await?,
    )?;
    let endpoint = format!("repos/{repo}/pulls/{number}/comments?per_page=100");
    let pages: Value = serde_json::from_str(
        &runner
            .run("gh", &["api", &endpoint, "--paginate", "--slurp"], cwd)
            .await?,
    )?;
    let mut snapshot = normalize_pr(url, &raw, &pages, local.root.clone())?;
    ensure!(
        snapshot.branch == local.branch,
        "local branch does not match the PR head branch"
    );
    let fresh: Value = serde_json::from_str(
        &runner
            .run(
                "gh",
                &["pr", "view", url, "--json", "headRefOid,headRefName,state"],
                cwd,
            )
            .await?,
    )?;
    ensure!(
        fresh["headRefOid"] == raw["headRefOid"]
            && fresh["headRefName"] == raw["headRefName"]
            && fresh["state"] == raw["state"],
        "PR changed during polling; baseline was not advanced"
    );
    let final_local = checkout_identity(runner, cwd).await?;
    ensure!(
        final_local == local,
        "local checkout changed during polling; reconfigure PR watch"
    );
    snapshot.cwd = local.root;
    snapshot.local_head = local.head;
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

pub(crate) fn normalize_pr(
    url: &str,
    raw: &Value,
    pages: &Value,
    cwd: PathBuf,
) -> Result<PrSnapshot> {
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
        url: url.trim_end_matches('/').to_owned(),
        cwd,
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
    use std::collections::VecDeque;

    const URL: &str = "https://github.com/example/repo/pull/1";

    struct FakeRunner {
        output: VecDeque<std::result::Result<String, String>>,
        calls: Vec<(String, Vec<String>, PathBuf)>,
    }

    impl FakeRunner {
        fn new(output: impl IntoIterator<Item = std::result::Result<String, String>>) -> Self {
            Self {
                output: output.into_iter().collect(),
                calls: Vec::new(),
            }
        }

        fn push_checkout(&mut self, root: &Path, branch: &str, head: &str) {
            self.output.extend([
                Ok(root.display().to_string()),
                Ok(branch.into()),
                Ok(head.into()),
            ]);
        }
    }

    impl PrCommandRunner for FakeRunner {
        fn run<'a>(
            &'a mut self,
            program: &'a str,
            args: &'a [&'a str],
            cwd: &'a Path,
        ) -> RunFuture<'a> {
            self.calls.push((
                program.into(),
                args.iter().map(|arg| (*arg).into()).collect(),
                cwd.to_path_buf(),
            ));
            let output = self
                .output
                .pop_front()
                .unwrap_or_else(|| Err("unexpected fake command".into()));
            Box::pin(async move { output.map_err(|error| eyre!(error)) })
        }
    }

    fn watch(cwd: &Path, url: Option<&str>) -> PrWatch {
        PrWatch {
            url: url.map(str::to_owned),
            cwd: cwd.to_path_buf(),
            driver: AgentId::new("driver"),
            branch: None,
            head: None,
            baseline: None,
            next_at: 0,
            error: None,
        }
    }

    fn observation_json() -> String {
        fixture().to_string()
    }

    fn discovered_view() -> String {
        json!({"url":URL,"state":"OPEN","headRefName":"feature"}).to_string()
    }

    fn pages() -> String {
        json!([[]]).to_string()
    }

    fn queue_discovery(runner: &mut FakeRunner, root: &Path, branch: &str, head: &str) {
        runner.push_checkout(root, branch, head);
        runner.output.push_back(Ok(discovered_view()));
        runner.push_checkout(root, branch, head);
        runner.output.push_back(Ok(observation_json()));
        runner.output.push_back(Ok(pages()));
        runner.output.push_back(Ok(
            json!({"headRefOid":"abc","headRefName":"feature","state":"OPEN"}).to_string(),
        ));
        runner.push_checkout(root, branch, head);
    }

    fn queue_pinned(runner: &mut FakeRunner, root: &Path, branch: &str, head: &str) {
        runner.push_checkout(root, branch, head);
        runner.push_checkout(root, branch, head);
        runner.output.push_back(Ok(observation_json()));
        runner.output.push_back(Ok(pages()));
        runner.output.push_back(Ok(
            json!({"headRefOid":"abc","headRefName":"feature","state":"OPEN"}).to_string(),
        ));
        runner.push_checkout(root, branch, head);
    }
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
            PathBuf::from("/fixture/repo"),
        )
        .unwrap();
        assert_eq!(snapshot.feedback.len(), 4);
        let (delta, baseline) = snapshot.delta(&snapshot.initial_baseline());
        assert_eq!(delta.len(), 1);
        assert!(snapshot.delta(&baseline).0.is_empty());
        let mut raw = fixture();
        raw["comments"][0]["body"] = json!("edited");
        let edited = normalize_pr(URL, &raw, &json!([[]]), PathBuf::from("/fixture/repo")).unwrap();
        assert_eq!(edited.delta(&baseline).0.len(), 1);
        raw["headRefOid"] = json!("def");
        assert_eq!(
            normalize_pr(URL, &raw, &json!([[]]), PathBuf::from("/fixture/repo"))
                .unwrap()
                .delta(&baseline)
                .0
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn discovers_current_open_pr_read_only_and_returns_pinned_root() {
        let checkout = tempfile::tempdir().unwrap();
        let nested = checkout.path().join("package");
        std::fs::create_dir(&nested).unwrap();
        let mut runner = FakeRunner::new([]);
        queue_discovery(&mut runner, checkout.path(), "feature", "local-sha");
        let snapshot = collect_pr_with(&watch(&nested, None), &mut runner)
            .await
            .unwrap();
        assert_eq!(snapshot.url, URL);
        assert_eq!(snapshot.cwd, checkout.path());
        assert_eq!(snapshot.branch, "feature");
        assert_eq!(snapshot.head, "abc");
        assert_eq!(snapshot.local_head, "local-sha");
        assert!(snapshot.open);
        let discovery = runner
            .calls
            .iter()
            .find(|(program, args, _)| {
                program == "gh" && args.starts_with(&["pr".into(), "view".into()])
            })
            .unwrap();
        assert_eq!(
            discovery.1,
            ["pr", "view", "--json", "url,headRefName,state"]
        );
        assert_eq!(runner.output.len(), 0);
        assert_eq!(runner.calls[0].2, nested);
        assert_eq!(
            runner.calls[3].2, nested,
            "branch lookup is from the original checkout context"
        );
        assert_eq!(
            runner.calls[4].2,
            checkout.path(),
            "collection pins the resolved Git root"
        );
    }

    #[tokio::test]
    async fn discovery_fails_closed_on_absent_closed_malformed_or_mismatched_pr() {
        let checkout = tempfile::tempdir().unwrap();
        for failure in [
            "no PR associated with this branch",
            "multiple PRs are associated with this branch",
            "authentication failed",
            "gh executable unavailable",
        ] {
            let mut runner = FakeRunner::new([Err(failure.into())]);
            assert!(
                discover_pr(&mut runner, checkout.path(), "feature")
                    .await
                    .is_err()
            );
        }
        for (raw, branch, expected) in [
            (
                json!({"url":URL,"state":"CLOSED","headRefName":"feature"}),
                "feature",
                "open",
            ),
            (
                json!({"url":"https://example.com/x","state":"OPEN","headRefName":"feature"}),
                "feature",
                "github.com",
            ),
            (
                json!({"url":URL,"state":"OPEN","headRefName":"other"}),
                "feature",
                "branch",
            ),
            (
                json!({"state":"OPEN","headRefName":"feature"}),
                "feature",
                "url",
            ),
        ] {
            let mut runner = FakeRunner::new([Ok(raw.to_string())]);
            let error = discover_pr(&mut runner, checkout.path(), branch)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.to_lowercase().contains(expected), "{error}");
        }
    }

    #[tokio::test]
    async fn pr_closing_between_discovery_and_full_collection_does_not_resolve_watch() {
        let checkout = tempfile::tempdir().unwrap();
        let mut runner = FakeRunner::new([]);
        queue_discovery(&mut runner, checkout.path(), "feature", "local-sha");
        let mut closed = fixture();
        closed["state"] = json!("CLOSED");
        runner.output[7] = Ok(closed.to_string());
        runner.output[9] =
            Ok(json!({"headRefOid":"abc","headRefName":"feature","state":"CLOSED"}).to_string());
        let error = collect_pr_with(&watch(checkout.path(), None), &mut runner)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("no longer open"), "{error}");
    }

    #[tokio::test]
    async fn refuses_detached_or_changing_local_checkout_during_discovery() {
        let checkout = tempfile::tempdir().unwrap();
        let mut detached =
            FakeRunner::new([Ok(checkout.path().display().to_string()), Ok("HEAD".into())]);
        assert!(
            checkout_identity(&mut detached, checkout.path())
                .await
                .is_err()
        );

        let mut changed_branch = FakeRunner::new([]);
        queue_discovery(&mut changed_branch, checkout.path(), "feature", "local-sha");
        changed_branch.output[5] = Ok("other".into());
        assert!(
            collect_pr_with(&watch(checkout.path(), None), &mut changed_branch)
                .await
                .is_err()
        );

        let mut changed_head = FakeRunner::new([]);
        queue_discovery(&mut changed_head, checkout.path(), "feature", "local-sha");
        changed_head.output[6] = Ok("new-local-sha".into());
        changed_head.output[12] = Ok("new-local-sha".into());
        assert!(
            collect_pr_with(&watch(checkout.path(), None), &mut changed_head)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn pinned_watch_never_runs_branch_discovery_again_and_rejects_head_races() {
        let checkout = tempfile::tempdir().unwrap();
        let mut runner = FakeRunner::new([]);
        queue_pinned(&mut runner, checkout.path(), "feature", "local-sha");
        let snapshot = collect_pr_with(&watch(checkout.path(), Some(URL)), &mut runner)
            .await
            .unwrap();
        assert_eq!(snapshot.url, URL);
        assert!(!runner.calls.iter().any(|(program, args, _)| {
            program == "gh"
                && args.iter().map(String::as_str).collect::<Vec<_>>()
                    == ["pr", "view", "--json", "url,headRefName,state"]
        }));
        assert!(
            runner
                .calls
                .iter()
                .filter(|(program, args, _)| {
                    program == "gh" && args.starts_with(&["pr".into(), "view".into()])
                })
                .all(|(_, args, _)| args.get(2).is_some_and(|target| target == URL))
        );

        let mut changed_remote = FakeRunner::new([]);
        queue_pinned(&mut changed_remote, checkout.path(), "feature", "local-sha");
        changed_remote.output[8] = Ok(
            json!({"headRefOid":"new-remote","headRefName":"feature","state":"OPEN"}).to_string(),
        );
        assert!(
            collect_pr_with(&watch(checkout.path(), Some(URL)), &mut changed_remote)
                .await
                .is_err()
        );
    }

    #[test]
    fn old_v3_pr_watch_url_string_deserializes_as_pinned() {
        let value = json!({
            "url": URL,
            "cwd": "/checkout",
            "driver": "driver",
            "branch": null,
            "head": null,
            "baseline": null,
            "next_at": 0,
            "error": null
        });
        let watch: PrWatch = serde_json::from_value(value).unwrap();
        assert_eq!(watch.url.as_deref(), Some(URL));
    }

    #[test]
    fn refuses_partial_data_and_untrusted_urls() {
        assert!(
            normalize_pr(
                URL,
                &fixture(),
                &json!([{}]),
                PathBuf::from("/fixture/repo")
            )
            .is_err()
        );
        for url in [
            "http://github.com/a/b/pull/1",
            "https://github.com/a/b/pull/1?x=1",
            "https://elsewhere/a/b/pull/1",
        ] {
            assert!(pr_target(url).is_err());
        }
    }
}
