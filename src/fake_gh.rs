//! Stand-in `gh` and `az` for the self-test and the parity suite:
//! `copilot-canvas.exe` copied as `gh.exe` / `az.exe` and run with
//! `COPILOT_CANVAS_FAKE_COPILOT=1` and `COPILOT_CANVAS_FAKE_PRS=<folder>`.
//! Each pull request is a fixture in that folder (`<key>.json`, see
//! [`Fixture`]) that a test edits (new comments, a failing check, merged);
//! the stand-in answers the canvas's read-only commands from it in the real
//! CLIs' JSON shapes and appends every call to `calls.log`. Anything else (a
//! comment, a vote, a merge) is refused and logged as `REFUSED`, so a test
//! notices a mutation. It never touches a network.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use canvas_agents::prs::PrRef;

/// The fixtures' folder.
pub const ENV: &str = "COPILOT_CANVAS_FAKE_PRS";

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct FakeComment {
    pub id: u64,
    pub author: String,
    pub body: String,
    /// An inline review comment (GitHub: from `gh api …/comments`).
    #[serde(default)]
    pub inline: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct FakeCheck {
    pub name: String,
    /// `SUCCESS`, `FAILURE`, `PENDING`.
    pub conclusion: String,
}

/// One pull request as the stand-in serves it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Fixture {
    /// `OPEN`, `MERGED`, `CLOSED` (Azure DevOps: active, completed, abandoned).
    pub state: String,
    pub title: String,
    pub author: String,
    pub head: String,
    #[serde(default)]
    pub comments: Vec<FakeComment>,
    /// (reviewer, `APPROVED` | `CHANGES_REQUESTED`).
    #[serde(default)]
    pub reviews: Vec<(String, String)>,
    #[serde(default)]
    pub checks: Vec<FakeCheck>,
    /// `auth`: every call fails as signed out.
    #[serde(default)]
    pub fail: String,
}

impl Fixture {
    pub fn open(title: &str, author: &str) -> Self {
        Fixture { state: "OPEN".into(), title: title.into(), author: author.into(), head: "1a2b3c4d5e6f".into(), ..Default::default() }
    }

    /// Adds `n` comments by `author` (ids follow the newest).
    pub fn comment(&mut self, n: usize, author: &str, text: &str) {
        for _ in 0..n {
            let id = self.comments.iter().map(|c| c.id).max().unwrap_or(1000) + 1;
            self.comments.push(FakeComment { id, author: author.into(), body: format!("{text} ({id})"), inline: id % 2 == 0 });
        }
    }
}

/// The fixture file of a pull request.
pub fn fixture_path(dir: &Path, pr: &PrRef) -> PathBuf {
    let name: String = pr.key().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    dir.join(format!("{name}.json"))
}

pub fn write_fixture(dir: &Path, pr: &PrRef, fixture: &Fixture) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = fixture_path(dir, pr);
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, serde_json::to_vec_pretty(fixture)?)?;
    std::fs::rename(&temp, &path)?;
    Ok(())
}

/// The calls the stand-ins answered (or refused), one per line.
pub fn calls(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("calls.log")).unwrap_or_default().lines().map(str::to_owned).collect()
}

/// This process is the stand-in `gh` or `az`.
pub fn active() -> Option<&'static str> {
    std::env::var_os(crate::fake_copilot::ENV)?;
    let stem = std::env::current_exe().ok()?.file_stem()?.to_string_lossy().to_ascii_lowercase();
    ["gh", "az"].into_iter().find(|n| *n == stem)
}

fn folder() -> Result<PathBuf> {
    std::env::var_os(ENV).map(PathBuf::from).context("COPILOT_CANVAS_FAKE_PRS is not set")
}

fn note(dir: &Path, line: &str) {
    if let Ok(mut log) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("calls.log")) {
        let _ = writeln!(log, "{line}");
    }
}

fn load(dir: &Path, pr: &PrRef) -> Result<Fixture> {
    let path = fixture_path(dir, pr);
    serde_json::from_slice(&std::fs::read(&path).with_context(|| format!("GraphQL: Could not resolve to a PullRequest ({})", path.display()))?).context("unreadable fixture")
}

/// Runs the stand-in `cli` with `args`; answers the exit code.
pub fn run(cli: &str, args: &[String]) -> i32 {
    let dir = match folder() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("{error:#}");
            return 1;
        }
    };
    let line = format!("{cli} {}", args.join(" "));
    let result = if cli == "gh" { gh(&dir, args) } else { az(&dir, args) };
    match result {
        Ok(out) => {
            note(&dir, &line);
            println!("{out}");
            0
        }
        Err(error) => {
            let text = format!("{error:#}");
            note(&dir, &format!("{}{line} -> {text}", if text.starts_with("stand-in") { "REFUSED " } else { "" }));
            eprintln!("{text}");
            1
        }
    }
}

fn arg<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.windows(2).find(|w| w[0] == name).map(|w| w[1].as_str())
}

fn gh(dir: &Path, args: &[String]) -> Result<String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["--version"] => Ok("gh version 0.0.0 (canvas stand-in)".into()),
        ["pr", "view", url, ..] => {
            let pr = PrRef::parse(url).map_err(|e| anyhow::anyhow!(e))?;
            let f = load(dir, &pr)?;
            if f.fail == "auth" {
                bail!("To get started with GitHub CLI, please run:  gh auth login\nAlternatively, populate the GH_TOKEN environment variable with a GitHub API authentication token.");
            }
            let comments: Vec<Value> = f.comments.iter().filter(|c| !c.inline).map(|c| json!({ "id": format!("IC_{}", c.id), "author": { "login": c.author }, "body": c.body, "url": format!("{}#issuecomment-{}", pr.url(), c.id), "isMinimized": false })).collect();
            let reviews: Vec<Value> = f.reviews.iter().enumerate().map(|(i, (who, state))| json!({ "id": format!("PRR_{i}"), "author": { "login": who }, "state": state, "body": "" })).collect();
            let checks: Vec<Value> = f
                .checks
                .iter()
                .map(|c| {
                    let done = c.conclusion != "PENDING";
                    json!({ "__typename": "CheckRun", "name": c.name, "status": if done { "COMPLETED" } else { "IN_PROGRESS" }, "conclusion": if done { c.conclusion.as_str() } else { "" }, "detailsUrl": format!("https://{}/{}/{}/actions/runs/1", pr.server, pr.owner, pr.repo) })
                })
                .collect();
            Ok(json!({
                "number": pr.number, "url": pr.url(), "title": f.title, "state": f.state, "isDraft": false, "headRefOid": f.head,
                "reviewDecision": "", "author": { "login": f.author }, "comments": comments, "reviews": reviews, "statusCheckRollup": checks,
            })
            .to_string())
        }
        ["api", rest @ ..] if rest.iter().any(|a| a.contains("/pulls/") && a.contains("/comments")) => {
            let path = rest.iter().find(|a| a.contains("/pulls/")).expect("checked");
            let parts: Vec<&str> = path.split('?').next().unwrap_or_default().split('/').collect();
            let [_, owner, repo, _, number, _] = parts[..] else { bail!("stand-in gh: unexpected api path {path}") };
            let host = arg(args, "--hostname").unwrap_or("github.com");
            let pr = PrRef::parse(&format!("https://{host}/{owner}/{repo}/pull/{number}")).map_err(|e| anyhow::anyhow!(e))?;
            let f = load(dir, &pr)?;
            let inline: Vec<Value> = f.comments.iter().filter(|c| c.inline).map(|c| json!({ "id": c.id, "user": { "login": c.author }, "body": c.body, "html_url": format!("{}#discussion_r{}", pr.url(), c.id), "path": "src/lib.rs" })).collect();
            Ok(Value::Array(inline).to_string())
        }
        _ => bail!("stand-in gh: only the canvas's read-only commands are answered"),
    }
}

/// The fixture an `az … --id N --org URL` call names.
fn ado_fixture(dir: &Path, args: &[String], id: &str) -> Result<(PrRef, Fixture)> {
    let org = arg(args, "--org").context("--org")?;
    let pr = PrRef::parse(&format!("{org}/_git/r/pullrequest/{id}")).map_err(|e| anyhow::anyhow!(e))?;
    let f = load(dir, &pr)?;
    if f.fail == "auth" {
        bail!("ERROR: Please run 'az login' to setup account.");
    }
    Ok((pr, f))
}

fn az(dir: &Path, args: &[String]) -> Result<String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["--version", ..] => Ok("azure-cli 0.0.0 (canvas stand-in)".into()),
        ["repos", "pr", "show", ..] => {
            let (_, f) = ado_fixture(dir, args, arg(args, "--id").context("--id")?)?;
            let status = match f.state.as_str() {
                "MERGED" => "completed",
                "CLOSED" => "abandoned",
                _ => "active",
            };
            let reviewers: Vec<Value> = f.reviews.iter().map(|(who, state)| json!({ "displayName": who, "uniqueName": format!("{who}@example.com"), "vote": if state == "APPROVED" { 10 } else { -5 } })).collect();
            Ok(json!({
                "pullRequestId": arg(args, "--id").unwrap_or_default().parse::<u64>().unwrap_or(0), "status": status, "isDraft": false, "title": f.title,
                "createdBy": { "displayName": f.author, "uniqueName": format!("{}@example.com", f.author) },
                "lastMergeSourceCommit": { "commitId": f.head }, "reviewers": reviewers,
                "repository": { "id": "repo-id", "name": "app", "project": { "id": "project-id", "name": "Web" } },
            })
            .to_string())
        }
        ["devops", "invoke", ..] if arg(args, "--resource") == Some("pullRequestThreads") && arg(args, "--http-method") == Some("GET") => {
            let id = args.iter().find_map(|a| a.strip_prefix("pullRequestId=")).context("pullRequestId")?;
            let (_, f) = ado_fixture(dir, args, id)?;
            let threads: Vec<Value> = f
                .comments
                .iter()
                .map(|c| json!({ "id": c.id, "isDeleted": false, "comments": [{ "id": 1, "author": { "displayName": c.author, "uniqueName": format!("{}@example.com", c.author) }, "content": c.body, "commentType": "text" }] }))
                .collect();
            Ok(json!({ "count": threads.len(), "value": threads }).to_string())
        }
        ["repos", "pr", "policy", "list", ..] => {
            let (_, f) = ado_fixture(dir, args, arg(args, "--id").context("--id")?)?;
            let policies: Vec<Value> = f
                .checks
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let status = match c.conclusion.as_str() {
                        "SUCCESS" => "approved",
                        "FAILURE" => "rejected",
                        _ => "running",
                    };
                    json!({ "status": status, "configuration": { "type": { "displayName": "Build" }, "settings": { "displayName": c.name, "buildDefinitionId": i + 1 } }, "context": { "buildId": 900 + i } })
                })
                .collect();
            Ok(Value::Array(policies).to_string())
        }
        _ => bail!("stand-in az: only the canvas's read-only commands are answered"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stand_ins_answer_in_the_shapes_the_canvas_parses_and_refuse_writes() {
        let dir = std::env::temp_dir().join(format!("cc-fake-gh-{}", uuid::Uuid::new_v4()));
        let gh_pr = PrRef::parse("https://github.com/octo/app/pull/42").unwrap();
        let mut f = Fixture::open("Fix flaky upload test", "octo-dev");
        f.comment(3, "ana", "please fix");
        f.checks.push(FakeCheck { name: "build".into(), conclusion: "FAILURE".into() });
        write_fixture(&dir, &gh_pr, &f).unwrap();
        let s = |v: &[&str]| v.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        let view = gh(&dir, &s(&["pr", "view", &gh_pr.url(), "--json", "x"])).unwrap();
        let inline = gh(&dir, &s(&["api", "--paginate", "repos/octo/app/pulls/42/comments?per_page=100"])).unwrap();
        let poll = canvas_agents::prs::parse_github(&view, &inline).unwrap();
        assert_eq!(poll.comments.len(), 3);
        assert_eq!(poll.checks[0].state, canvas_agents::prs::CheckState::Fail);
        assert!(gh(&dir, &s(&["pr", "comment", &gh_pr.url(), "--body", "x"])).unwrap_err().to_string().starts_with("stand-in gh"), "writes are refused");
        let ado_pr = PrRef::parse("https://dev.azure.com/contoso/Web/_git/app/pullrequest/77").unwrap();
        f.state = "MERGED".into();
        write_fixture(&dir, &ado_pr, &f).unwrap();
        let common = ["--org", "https://dev.azure.com/contoso", "--output", "json"];
        let show = az(&dir, &[s(&["repos", "pr", "show", "--id", "77"]), s(&common)].concat()).unwrap();
        let threads = az(&dir, &[s(&["devops", "invoke", "--area", "git", "--resource", "pullRequestThreads", "--route-parameters", "project=p", "repositoryId=r", "pullRequestId=77", "--http-method", "GET"]), s(&common)].concat()).unwrap();
        let policies = az(&dir, &[s(&["repos", "pr", "policy", "list", "--id", "77"]), s(&common)].concat()).unwrap();
        let poll = canvas_agents::prs::parse_ado(&ado_pr, &show, &threads, &policies).unwrap();
        assert_eq!((poll.state, poll.comments.len(), poll.repo.as_str()), (canvas_agents::prs::PrState::Merged, 3, "app"));
        f.fail = "auth".into();
        write_fixture(&dir, &gh_pr, &f).unwrap();
        assert!(gh(&dir, &s(&["pr", "view", &gh_pr.url()])).unwrap_err().to_string().contains("gh auth login"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
