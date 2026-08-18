//! The long tail of the CLI, as a table.
//!
//! Most read and admin endpoints are the same twelve lines: validate a couple
//! of parameters, build an argv, run it in the project root, return stdout.
//! Writing that forty times would be forty chances to forget the allowlist or
//! the ceiling. So the routes are data — a table of
//! `(method, path) -> subcommand + accepted parameters` — and one dispatcher
//! executes them.
//!
//! The table is also what `GET /v1/schema` publishes, so the documented
//! surface and the served surface cannot disagree: they are the same array.
//!
//! Parameters are an allowlist per route. A query key the route does not
//! declare is ignored rather than appended, so no client can turn
//! `?annotate=1` into an extra flag on a command that never expected one.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::net::TcpStream;

use super::http::{self, Request};
use super::projects::Project;
use super::{argv, child, ServeState};

/// How a declared query parameter becomes an argument.
#[derive(Debug, Clone, Copy)]
pub enum Param {
    /// `?compare` / `?compare=true` → `--compare`.
    Flag {
        query: &'static str,
        flag: &'static str,
    },
    /// `?base=main` → `--local main`.
    Value {
        query: &'static str,
        flag: &'static str,
    },
    /// `?pr=42` → a bare positional argument.
    Positional { query: &'static str },
}

impl Param {
    fn query(&self) -> &'static str {
        match self {
            Param::Flag { query, .. } | Param::Value { query, .. } => query,
            Param::Positional { query } => query,
        }
    }
}

pub struct Route {
    pub method: &'static str,
    /// Path under `/v1/projects/{project}/`.
    pub path: &'static [&'static str],
    /// Subcommand and fixed arguments.
    pub argv: &'static [&'static str],
    pub params: &'static [Param],
    pub about: &'static str,
}

/// Every table-driven route. Project-scoped without exception: even
/// "global" state like skills or config is reported from inside a repo,
/// because the merged view depends on which repo you are in.
pub const ROUTES: &[Route] = &[
    Route {
        method: "GET",
        path: &["cost"],
        argv: &["cost", "--json"],
        params: &[Param::Flag {
            query: "compare",
            flag: "--compare",
        }],
        about: "token spend by model; ?compare reprices it against other models",
    },
    Route {
        method: "GET",
        path: &["context"],
        argv: &["context", "--json"],
        params: &[],
        about: "per-turn context tax: system prompt and tool schema token counts",
    },
    Route {
        method: "GET",
        path: &["knows"],
        argv: &["knows"],
        params: &[],
        about: "what Wingman knows about this project",
    },
    Route {
        method: "GET",
        path: &["doctor"],
        argv: &["doctor"],
        params: &[],
        about: "health check: credentials, servers, index, tooling",
    },
    Route {
        method: "GET",
        path: &["attest"],
        argv: &["attest"],
        params: &[],
        about: "air-gapped / local-only posture report",
    },
    Route {
        method: "GET",
        path: &["diff"],
        argv: &["diff"],
        params: &[Param::Positional { query: "file" }],
        about: "working-tree diff, optionally for one ?file",
    },
    Route {
        method: "GET",
        path: &["explain"],
        argv: &["explain"],
        params: &[
            Param::Value {
                query: "base",
                flag: "--local",
            },
            Param::Flag {
                query: "staged",
                flag: "--staged",
            },
        ],
        about: "explain the current changes in prose",
    },
    Route {
        method: "GET",
        path: &["review"],
        argv: &["review"],
        params: &[
            Param::Positional { query: "pr" },
            Param::Value {
                query: "base",
                flag: "--local",
            },
        ],
        about: "review a PR (?pr=42) or local commits against ?base",
    },
    Route {
        method: "GET",
        path: &["router", "stats"],
        argv: &["router", "stats"],
        params: &[Param::Flag {
            query: "all",
            flag: "--all",
        }],
        about: "per-class model win rates for this repo",
    },
    Route {
        method: "GET",
        path: &["index", "status"],
        argv: &["indexd", "--status"],
        params: &[],
        about: "semantic index freshness and whether indexd is running",
    },
    Route {
        method: "GET",
        path: &["trust"],
        argv: &["trust", "show"],
        params: &[],
        about: "is this project's config trusted",
    },
    Route {
        method: "POST",
        path: &["trust"],
        argv: &["trust", "add"],
        params: &[],
        about: "trust this project's config as it currently stands",
    },
    Route {
        method: "GET",
        path: &["memory"],
        argv: &["memory", "review"],
        params: &[],
        about: "pending distilled memories awaiting review",
    },
    Route {
        method: "POST",
        path: &["memory", "sync"],
        argv: &["memory", "sync"],
        params: &[Param::Positional { query: "ref" }],
        about: "rebuild MEMORY.md, optionally folding in memories from a git ref",
    },
    Route {
        method: "POST",
        path: &["checkpoints"],
        argv: &["checkpoint"],
        params: &[Param::Value {
            query: "label",
            flag: "--label",
        }],
        about: "stash the working tree as a recoverable checkpoint",
    },
    Route {
        method: "POST",
        path: &["rewind"],
        argv: &["rewind"],
        params: &[Param::Positional { query: "steps" }],
        about: "revert the last ?steps mutating edits; omit to print the timeline",
    },
    Route {
        method: "POST",
        path: &["index", "reindex"],
        argv: &["indexd"],
        params: &[],
        about: "rebuild the semantic index (runs until interrupted; use the timeout)",
    },
    Route {
        method: "POST",
        path: &["schedule", "run"],
        argv: &["schedule"],
        params: &[Param::Flag {
            query: "all",
            flag: "--all",
        }],
        about: "run due scheduled prompts (?all forces every entry)",
    },
    Route {
        method: "GET",
        path: &["config"],
        argv: &["config", "show", "--json"],
        params: &[],
        about: "merged effective configuration",
    },
];

/// Find the route matching this request, if any.
pub fn find(method: &str, path: &[&str]) -> Option<&'static Route> {
    ROUTES.iter().find(|r| r.method == method && r.path == path)
}

/// Build the argv for `route` from the request's declared parameters.
///
/// Undeclared query keys are dropped. Values are passed as separate argv
/// entries, never interpolated into a string, so a value containing spaces or
/// shell metacharacters is just a value.
fn build_argv(route: &Route, req: &Request) -> Vec<String> {
    let mut out: Vec<String> = route.argv.iter().map(|s| s.to_string()).collect();
    for param in route.params {
        match param {
            Param::Flag { query, flag } => {
                if req.query_bool(query) {
                    out.push((*flag).to_string());
                }
            }
            Param::Value { query, flag } => {
                if let Some(v) = req.query_str(query) {
                    if !v.is_empty() {
                        out.push((*flag).to_string());
                        out.push(v.to_string());
                    }
                }
            }
            Param::Positional { query } => {
                if let Some(v) = req.query_str(query) {
                    if !v.is_empty() {
                        out.push(v.to_string());
                    }
                }
            }
        }
    }
    out
}

/// Run a table route and answer with the command's output.
pub async fn run(
    state: &Arc<ServeState>,
    project: &Project,
    route: &Route,
    req: &Request,
    sock: &mut TcpStream,
) -> std::io::Result<()> {
    let args = build_argv(route, req);
    // Through the same gate `/v1/exec` uses: one allowlist, one ceiling
    // check, no second path to audit.
    let args = match argv::sanitize(&args, state.ceiling) {
        Ok(a) => a,
        Err(argv::Rejected::BadRequest(m)) => return http::write_err(sock, 400, &m).await,
        Err(argv::Rejected::Forbidden(m)) => return http::write_err(sock, 403, &m).await,
    };
    execute(state, project, &args, sock).await
}

/// Spawn `wingman <args>` in the project and return its output.
///
/// Stdout that parses as JSON is returned as JSON — the commands with a
/// `--json` mode are the ones worth machine-reading, and re-wrapping their
/// output in a string would make every client parse twice. Everything else
/// comes back as `{stdout, stderr, exit}`, which is honest about being text.
pub async fn execute(
    state: &Arc<ServeState>,
    project: &Project,
    args: &[String],
    sock: &mut TcpStream,
) -> std::io::Result<()> {
    let mut cmd = match child::command(&project.root, state.ceiling) {
        Ok(c) => c,
        Err(e) => return http::write_err(sock, 500, &format!("resolving executable: {e}")).await,
    };
    cmd.args(args);

    let timeout = Duration::from_secs(state.cfg.serve.request_timeout_secs.max(1));
    match child::run_to_completion(cmd, timeout).await {
        Ok(out) => {
            let status = if out.code == 0 { 200 } else { 500 };
            match serde_json::from_str::<Value>(out.stdout.trim()) {
                Ok(parsed) if out.code == 0 => http::write_json(sock, 200, &parsed).await,
                _ => {
                    http::write_json(
                        sock,
                        status,
                        &json!({
                            "stdout": out.stdout,
                            "stderr": out.stderr,
                            "exit": out.code,
                        }),
                    )
                    .await
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            http::write_err(
                sock,
                504,
                "command timed out ([serve].request_timeout_secs)",
            )
            .await
        }
        Err(e) => http::write_err(sock, 500, &format!("running command: {e}")).await,
    }
}

/// Schema fragment for `GET /v1/schema`, generated from the same table the
/// dispatcher matches against.
pub fn schema() -> Vec<Value> {
    ROUTES
        .iter()
        .map(|r| {
            let params: Vec<&str> = r.params.iter().map(|p| p.query()).collect();
            json!({
                "method": r.method,
                "path": format!("/v1/projects/{{project}}/{}", r.path.join("/")),
                "auth": true,
                "runs": format!("wingman {}", r.argv.join(" ")),
                "params": params,
                "about": r.about,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(target: &str) -> Request {
        // Only the query matters for argv building; the transport's own tests
        // cover parsing.
        let raw = format!("GET {target} HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n");
        super::super::http::parse_for_test(&raw)
    }

    #[test]
    fn declared_flags_are_appended_and_undeclared_keys_ignored() {
        let route = find("GET", &["cost"]).unwrap();
        assert_eq!(
            build_argv(route, &request("/v1/projects/p/cost?compare=1")),
            vec!["cost", "--json", "--compare"]
        );
        // `annotate` is not declared on this route, so it contributes nothing.
        assert_eq!(
            build_argv(route, &request("/v1/projects/p/cost?annotate=1&--yolo=1")),
            vec!["cost", "--json"]
        );
    }

    #[test]
    fn values_and_positionals_stay_separate_argv_entries() {
        let route = find("GET", &["review"]).unwrap();
        let args = build_argv(route, &request("/v1/projects/p/review?pr=42&base=main"));
        assert_eq!(args, vec!["review", "42", "--local", "main"]);

        // A value with spaces and shell metacharacters is one argument, not a
        // command line — there is no shell in this path at all.
        let route = find("POST", &["checkpoints"]).unwrap();
        let args = build_argv(
            route,
            &request("/v1/projects/p/checkpoints?label=before%20the%20%3B%20rm%20-rf"),
        );
        assert_eq!(args, vec!["checkpoint", "--label", "before the ; rm -rf"]);
    }

    #[test]
    fn empty_values_are_dropped_rather_than_passed_as_empty_args() {
        let route = find("GET", &["review"]).unwrap();
        assert_eq!(
            build_argv(route, &request("/v1/projects/p/review?pr=")),
            vec!["review"]
        );
    }

    #[test]
    fn every_route_survives_the_argv_gate() {
        // A table entry that the allowlist would reject is a route that 500s
        // in production and passes review unnoticed.
        for route in ROUTES {
            let args: Vec<String> = route.argv.iter().map(|s| s.to_string()).collect();
            assert!(
                argv::sanitize(&args, wingman_config::PermissionMode::AutoEdit).is_ok(),
                "route {} {:?} is refused by the argv gate",
                route.method,
                route.path
            );
        }
    }

    #[test]
    fn schema_covers_every_route() {
        assert_eq!(schema().len(), ROUTES.len());
    }
}
