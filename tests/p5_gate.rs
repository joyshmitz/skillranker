#![cfg(any(target_os = "linux", target_os = "macos"))]
//! Phase P5 Acceptance Gate: local evidence, faithful replay and bounded
//! evaluation, integrated at one revision.
//!
//! Satisfies contract boundary `p5_acceptance_gate` (sr-roadmap-l1i.6.29)
//! mapped in `tests/contract_matrix.toml`.
//!
//! One isolated home runs the whole P5 chain through the real binary against
//! the loopback TLS Jev fixture (synthetic data only): ledger init, a recorded
//! live ranking, case capture and offline replay of it, explicit feedback,
//! statistics, labeled-frame evaluation with explanations, and a budgeted live
//! evaluation batch. Every quality gate stays not-established: a working
//! evaluation command is not a policy meeting relevance or harm thresholds.

mod support;

use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Gate {
    root: PathBuf,
}

impl Gate {
    fn new() -> Self {
        // Owner-only under sticky /tmp: the ledger and cache refuse stores
        // with group-writable ancestors, as an RCH TMPDIR can have.
        let root = support::private_store_dir("p5-gate");
        for dir in ["home", "config", "cache", "data", "workspace"] {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(root.join(dir))
                .unwrap();
        }
        for (name, description) in [
            ("alpha", "Runs and repairs failing rust tests."),
            ("beta", "Drafts release notes from git history."),
        ] {
            let dir = root.join("workspace/.claude/skills").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {description}\n---\nBody.\n"),
            )
            .unwrap();
        }
        std::fs::write(
            root.join("fixture-ca.pem"),
            include_bytes!("fixtures/jev-tls/ca.pem"),
        )
        .unwrap();
        Self { root }
    }

    fn path(&self, name: &str) -> String {
        self.root
            .join("workspace")
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    fn run(&self, port: u16, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sr"))
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("TYPESAFE_API_KEY", "synthetic-p5-gate-canary")
            .env("TYPESAFE_ENDPOINT", format!("https://localhost:{port}"))
            .env("SSL_CERT_FILE", self.root.join("fixture-ca.pem"))
            .current_dir(self.root.join("workspace"))
            .args(args)
            .output()
            .unwrap()
    }

    fn json(&self, port: u16, args: &[&str]) -> Value {
        let out = self.run(port, args);
        assert!(
            out.status.success(),
            "{args:?}: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn context(&self, session: &str) -> Value {
        json!({
            "schema_version": 1, "harness": "claude_code", "producer_id": "p5-gate",
            "workspace_root": self.root.join("workspace").to_str().unwrap(),
            "session_id": session, "agent_id": null, "branch_id": null, "context_epoch": null,
            "current_request": {"event_id": format!("{session}-request"),
                "text": "Our rust test suite started failing; find out why.",
                "attachments_omitted": false, "essential_attachment_missing": false},
            "events": [], "explicit_skill_references": [], "supplied_loads": []
        })
    }

    fn write(&self, name: &str, rows: &[Value]) -> String {
        let path = self.path(name);
        let body: String = rows.iter().map(|row| format!("{row}\n")).collect();
        std::fs::write(&path, body).unwrap();
        path
    }
}

/// The loopback TLS Jev fixture, as the rank and eval integration tests use it.
struct Provider {
    child: Child,
    lines: BufReader<ChildStdout>,
    port: u16,
}

impl Provider {
    fn start(gate: &Gate, scenario: &str) -> Self {
        let dir = gate
            .root
            .join(format!("provider-{}", NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir(&dir).unwrap();
        for (name, bytes) in [
            (
                "provider_server.py",
                &include_bytes!("fixtures/jev-tls/provider_server.py")[..],
            ),
            (
                "server.pem",
                &include_bytes!("fixtures/jev-tls/server.pem")[..],
            ),
            (
                "server.key",
                &include_bytes!("fixtures/jev-tls/server.key")[..],
            ),
        ] {
            std::fs::write(dir.join(name), bytes).unwrap();
        }
        let mut child = Command::new("/usr/bin/python3")
            .arg(dir.join("provider_server.py"))
            .arg(scenario)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap());
        let mut hello = String::new();
        lines.read_line(&mut hello).unwrap();
        let hello: Value = serde_json::from_str(&hello).unwrap();
        let port = u16::try_from(hello["port"].as_u64().unwrap()).unwrap();
        Self { child, lines, port }
    }

    /// Requests the provider answered.
    fn finish(mut self) -> usize {
        let mut done = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        std::io::Write::write_all(&mut done, b"DONE").unwrap();
        drop(done);
        let mut served = 0;
        loop {
            let mut line = String::new();
            assert!(
                self.lines.read_line(&mut line).unwrap() > 0,
                "provider ended early"
            );
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["done"] == true {
                break;
            }
            served += usize::from(value["handshake_rejected"] != true);
        }
        let _ = self.child.wait();
        served
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn frame_key(case: &str) -> Value {
    json!({"frame_id": "p5-gate", "family_id": format!("fam-{case}"), "case_id": case,
           "replicate": 0, "policy_id": "gate"})
}

fn label(case: &str, acceptable: &[&str]) -> Value {
    json!({"schema_version": 1, "case_id": case, "revision": 1,
           "acceptable_skills": acceptable, "no_skill_needed": acceptable.is_empty(),
           "adjudicator": "gate-judge", "created_at_unix_ms": 1_726_700_000_000u64})
}

#[test]
fn all_p5_contracts_verified() {
    let gate = Gate::new();

    // 1. Storage: an explicit, idempotent ledger initialization.
    gate.json(1, &["ledger", "init", "--json"]);
    gate.json(1, &["ledger", "init", "--json"]);

    // 2. A live ranking through TLS is recorded and captured as a case.
    let context = gate.path("context.json");
    std::fs::write(&context, gate.context("p5-session").to_string()).unwrap();
    let case_file = gate.path("case.json");
    let provider = Provider::start(&gate, "useful");
    let ranked = gate.json(
        provider.port,
        &[
            "rank",
            "--context",
            &context,
            "--allow-network",
            "--timeout-ms",
            "12000",
            "--save-case",
            &case_file,
            "--json",
        ],
    );
    assert_eq!(provider.finish(), 2, "one wide and one rerank request");
    assert_eq!(ranked["decision"], "ranked", "{ranked}");
    let event_id = ranked["event_id"]
        .as_str()
        .expect("a recorded event")
        .to_owned();
    let top_id = ranked["skills"][0]["skill_id"].as_str().unwrap().to_owned();

    // 3. Faithful replay: offline, no request, the historical decision recomputed.
    let replay = gate.json(1, &["replay", &case_file, "--json"]);
    assert_eq!(replay["kind"], "replay", "{replay}");
    assert_eq!(replay["actionable"], false);
    assert_eq!(replay["historical"]["decision"], "ranked", "{replay}");
    assert_eq!(replay["recomputed"]["decision"], "ranked", "{replay}");
    assert_eq!(
        replay["recomputed"]["skills"][0]["skill_id"], top_id,
        "replay must reproduce the recorded top skill: {replay}"
    );

    // 4. Explicit feedback on the recorded event, by its retained stable ID
    //    (the discovered roster is partial here: no user skill root exists),
    //    then statistics that see it.
    gate.json(
        1,
        &[
            "feedback",
            &event_id,
            "--skill",
            &top_id,
            "--verdict",
            "useful",
            "--json",
        ],
    );
    let stats = gate.json(1, &["stats", "--json"]);
    assert_eq!(stats["judgments"]["total_judgments"], 1, "{stats}");
    assert_eq!(stats["judgments"]["useful"], 1, "{stats}");

    // 5. A labeled frame of recorded decisions, scored and explained offline.
    let frame = gate.write(
        "frame.jsonl",
        &[
            json!({"schema_version": 1, "key": frame_key("recorded"), "split": "holdout",
                 "prompt_summary": "failing rust tests", "decision": "ranked",
                 "suggested_skills": [top_id]}),
        ],
    );
    let labels = gate.write(
        "labels.jsonl",
        &[
            label("recorded", &[top_id.as_str()]),
            label("live", &[top_id.as_str()]),
        ],
    );
    let scored = gate.json(
        1,
        &[
            "eval",
            "--dataset",
            &frame,
            "--labels",
            &labels,
            "--explain",
            "--json",
        ],
    );
    assert_eq!(scored["kind"], "report");
    assert_eq!(scored["gate_status"], "not-established", "{scored}");
    assert_eq!(scored["loss_summary"]["total_loss"], 0);
    assert!(
        scored["explanation"]["quantities"]
            .as_array()
            .unwrap()
            .len()
            >= 2
    );

    // 6. A budgeted live batch: preflight before any send, exact accounting,
    //    baselines and the review queue, and still no passed gate.
    let live = gate.write(
        "live.jsonl",
        &[
            json!({"schema_version": 1, "key": frame_key("live"), "split": "holdout",
                 "context": gate.context("p5-live")}),
        ],
    );
    let provider = Provider::start(&gate, "useful");
    let batch = gate.json(
        provider.port,
        &[
            "eval",
            "--dataset",
            &live,
            "--labels",
            &labels,
            "--online",
            "--allow-network",
            "--max-requests",
            "4",
            "--json",
        ],
    );
    let served = provider.finish();
    assert_eq!(batch["evidence_origin"], "live", "{batch}");
    assert_eq!(batch["run_status"], "complete", "{batch}");
    assert_eq!(batch["gate_status"], "not-established");
    assert_eq!(batch["accounting"]["http_attempts"], served as u64);
    assert_eq!(batch["disclosure_preflight"]["cases_checked"], 1);
    assert!(batch["baselines"]["policies"].as_array().unwrap().len() >= 4);
    assert_eq!(batch["review_queue"]["denominator_effect"], "none");
    // The live batch wrote nothing: the ledger still holds exactly the one
    // recorded ranking and its judgment.
    // The report's time window and retention cutoff move with the clock, not
    // the ledger: compare every count, ignoring only wall-clock fields.
    fn without_clock(value: Value) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .filter(|(key, _)| !key.ends_with("_unix_ms"))
                    .map(|(key, value)| (key, without_clock(value)))
                    .collect(),
            ),
            Value::Array(items) => Value::Array(items.into_iter().map(without_clock).collect()),
            other => other,
        }
    }
    let window_free = without_clock;
    let after = gate.json(1, &["stats", "--json"]);
    assert_eq!(
        window_free(after),
        window_free(stats),
        "a live evaluation batch must not touch the ledger"
    );
}
