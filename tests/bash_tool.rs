use std::time::{Duration, Instant};

use garde::Validate;
use moh::tools::{BashArgs, BashServiceFactory, JobRegistry, JobState};
use schemars::schema_for;
use serde_json::json;

fn service(directory: &std::path::Path) -> (JobRegistry, moh::tools::BashService) {
    let registry = JobRegistry::new();
    let service = BashServiceFactory::new(registry.clone()).for_cwd(directory.to_owned());
    (registry, service)
}

fn full_output_path(text: &str) -> &std::path::Path {
    let path = text
        .lines()
        .find_map(|line| line.strip_prefix("Full output: "))
        .expect("truncated output should expose the complete log");
    std::path::Path::new(path)
}

#[tokio::test]
async fn foreground_bash_runs_from_the_bound_cwd_and_preserves_nonzero_exit() {
    let directory = tempfile::tempdir().unwrap();
    let (_registry, bash) = service(directory.path());

    let output = bash
        .bash(BashArgs {
            command: "printf '%s\\n' \"$PWD\"; printf 'warning\\n' >&2; exit 7".into(),
            background: false,
            timeout_ms: None,
        })
        .await
        .unwrap();
    let text = output.as_text().unwrap();

    assert!(text.contains(directory.path().to_str().unwrap()));
    assert!(text.contains("[stderr] warning"));
    assert!(text.contains("state: completed"));
    assert!(text.contains("exit code: 7"));
}

#[tokio::test]
async fn background_bash_returns_before_the_process_finishes() {
    let directory = tempfile::tempdir().unwrap();
    let (registry, bash) = service(directory.path());
    let started = Instant::now();

    let output = bash
        .bash(BashArgs {
            command: "sleep 0.5; printf 'done\\n'".into(),
            background: true,
            timeout_ms: None,
        })
        .await
        .unwrap();

    assert!(started.elapsed() < Duration::from_millis(250));
    assert!(output.as_text().unwrap().contains("state: running"));
    let terminal = registry
        .wait(&["job-0".parse().unwrap()], Some(Duration::from_secs(1)))
        .await
        .unwrap();
    assert_eq!(terminal.snapshots[0].state(), JobState::Completed);
}

#[tokio::test]
async fn bash_rejects_empty_commands_and_out_of_range_timeouts() {
    let directory = tempfile::tempdir().unwrap();
    let (_registry, bash) = service(directory.path());

    for args in [
        BashArgs {
            command: String::new(),
            background: false,
            timeout_ms: None,
        },
        BashArgs {
            command: "true".into(),
            background: false,
            timeout_ms: Some(0),
        },
        BashArgs {
            command: "true".into(),
            background: false,
            timeout_ms: Some(3_600_001),
        },
    ] {
        let error = bash.bash(args).await.unwrap_err();
        assert!(error.to_string().starts_with("[E_INVALID_ARGUMENT]"));
    }
}

#[tokio::test]
async fn bash_inherits_the_service_environment() {
    let directory = tempfile::tempdir().unwrap();
    let (_registry, bash) = service(directory.path());
    let variable = format!("MOH_BASH_INHERIT_{}", std::process::id());
    // SAFETY: this test uses a unique variable name and no other thread reads it.
    unsafe { std::env::set_var(&variable, "inherited-value") };
    let output = bash
        .bash(BashArgs {
            command: format!("printf '%s' \"${variable}\""),
            background: false,
            timeout_ms: None,
        })
        .await
        .unwrap();

    unsafe { std::env::remove_var(&variable) };
    assert!(
        output
            .as_text()
            .unwrap()
            .contains("[stdout] inherited-value")
    );
}

#[tokio::test]
async fn capacity_exhaustion_is_model_visible_busy() {
    let directory = tempfile::tempdir().unwrap();
    let (registry, bash) = service(directory.path());
    let _leases = (0..16)
        .map(|_| {
            registry
                .start(
                    moh::tools::JobKind::Bash,
                    "occupied",
                    std::sync::Arc::new(TestJobDetails),
                )
                .unwrap()
        })
        .collect::<Vec<_>>();

    let error = bash
        .bash(BashArgs {
            command: "true".into(),
            background: false,
            timeout_ms: None,
        })
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "[E_BUSY] too many Bash jobs are running");
}

#[derive(Debug)]
struct TestJobDetails;

impl moh::tools::JobDetails for TestJobDetails {
    fn render(&self) -> String {
        "occupied".into()
    }
}

#[tokio::test]
async fn split_multibyte_output_and_partial_lines_are_preserved_logically() {
    let directory = tempfile::tempdir().unwrap();
    let (_registry, bash) = service(directory.path());
    let output = bash
        .bash(BashArgs {
            command: r"printf '%04095d' 0 | tr 0 x; printf '\342'; sleep 0.02; printf '\202\254tail'; sleep 0.02; printf '\n'".into(),
            background: false,
            timeout_ms: None,
        })
        .await
        .unwrap();
    let text = output.as_text().unwrap();

    assert!(text.contains(&format!("[stdout] {}€tail", "x".repeat(4095))));
    assert!(!text.contains('�'));
}

#[tokio::test]
async fn invalid_utf8_is_decoded_lossily_without_dropping_following_text() {
    let directory = tempfile::tempdir().unwrap();
    let (_registry, bash) = service(directory.path());
    let output = bash
        .bash(BashArgs {
            command: r"printf 'before\377after'".into(),
            background: false,
            timeout_ms: None,
        })
        .await
        .unwrap();

    assert!(output.as_text().unwrap().contains("[stdout] before�after"));
}

#[tokio::test]
async fn output_tail_keeps_exactly_two_thousand_logical_lines() {
    let directory = tempfile::tempdir().unwrap();
    let (_registry, bash) = service(directory.path());
    let output = bash
        .bash(BashArgs {
            command: "for i in $(seq 1 2001); do printf 'l%04d\\n' \"$i\"; done".into(),
            background: false,
            timeout_ms: None,
        })
        .await
        .unwrap();
    let details = output.as_text().unwrap().split("details: ").nth(1).unwrap();
    let retained = details
        .lines()
        .filter_map(|line| line.find("[stdout] l").map(|start| &line[start..]))
        .collect::<Vec<_>>();
    assert_eq!(retained.len(), 2_000);
    assert_eq!(retained.first().copied(), Some("[stdout] l0002"));
    assert_eq!(retained.last().copied(), Some("[stdout] l2001"));
}

#[tokio::test]
async fn oversized_multibyte_line_respects_byte_cap_at_character_boundary() {
    let directory = tempfile::tempdir().unwrap();
    let (_registry, bash) = service(directory.path());
    let output = bash
        .bash(BashArgs {
            command: "for _ in $(seq 1 18000); do printf '€'; done; printf '\\n'".into(),
            background: false,
            timeout_ms: None,
        })
        .await
        .unwrap();
    let line = output
        .as_text()
        .unwrap()
        .lines()
        .find_map(|line| line.find("[stdout] ").map(|start| &line[start..]))
        .unwrap();
    assert!(line.len() < 50 * 1024);
    assert!(!line.contains('�'));
}

#[test]
fn bash_args_are_strict_and_schema_has_the_expected_defaults_and_bounds() {
    let defaulted: BashArgs = serde_json::from_value(json!({"command": "true"})).unwrap();
    assert!(!defaulted.background);
    assert_eq!(defaulted.timeout_ms, None);
    assert!(serde_json::from_value::<BashArgs>(json!({"background": false})).is_err());
    assert!(
        serde_json::from_value::<BashArgs>(json!({"command": "true", "unexpected": true})).is_err()
    );

    assert!(
        BashArgs {
            command: String::new(),
            background: false,
            timeout_ms: None,
        }
        .validate()
        .is_err()
    );
    assert!(
        BashArgs {
            command: "true".into(),
            background: false,
            timeout_ms: Some(0),
        }
        .validate()
        .is_err()
    );
    assert!(
        BashArgs {
            command: "true".into(),
            background: false,
            timeout_ms: Some(3_600_001),
        }
        .validate()
        .is_err()
    );

    let schema = serde_json::to_value(schema_for!(BashArgs)).unwrap();
    assert_eq!(schema["required"], json!(["command"]));
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["properties"]["background"]["default"], false);
    assert_eq!(schema["properties"]["timeout_ms"]["minimum"], 1);
    assert_eq!(schema["properties"]["timeout_ms"]["maximum"], 3_600_000);
}

#[tokio::test]
async fn timeout_is_failed_and_preserves_partial_output() {
    let directory = tempfile::tempdir().unwrap();
    let (_registry, bash) = service(directory.path());
    let output = bash
        .bash(BashArgs {
            command: "printf 'before timeout\\n'; sleep 30".into(),
            background: false,
            timeout_ms: Some(30),
        })
        .await
        .unwrap();
    let text = output.as_text().unwrap();
    assert!(text.contains("state: failed"));
    assert!(text.contains("timeout after 30 ms"));
    assert!(text.contains("before timeout"));
}

#[tokio::test]
async fn shutdown_cancels_a_started_background_job_and_rejects_new_starts() {
    let directory = tempfile::tempdir().unwrap();
    let (registry, bash) = service(directory.path());
    bash.bash(BashArgs {
        command: "sleep 30".into(),
        background: true,
        timeout_ms: None,
    })
    .await
    .unwrap();

    registry.shutdown().await.unwrap();

    let snapshot = registry.status(Some("job-0".parse().unwrap())).unwrap()[0].clone();
    assert_eq!(snapshot.state(), JobState::Cancelled);
    assert!(
        bash.bash(BashArgs {
            command: "true".into(),
            background: true,
            timeout_ms: None,
        })
        .await
        .is_err()
    );
}

#[tokio::test]
async fn aborting_a_foreground_request_cancels_its_job() {
    let directory = tempfile::tempdir().unwrap();
    let (registry, bash) = service(directory.path());
    let request = tokio::spawn(async move {
        bash.bash(BashArgs {
            command: "sleep 30".into(),
            background: false,
            timeout_ms: None,
        })
        .await
    });
    while registry.status(None).unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    request.abort();

    let terminal = registry
        .wait(&["job-0".parse().unwrap()], Some(Duration::from_secs(3)))
        .await
        .unwrap();
    assert_eq!(terminal.snapshots[0].state(), JobState::Cancelled);
}

#[tokio::test]
async fn returned_background_job_survives_the_originating_request() {
    let directory = tempfile::tempdir().unwrap();
    let (registry, bash) = service(directory.path());
    bash.bash(BashArgs {
        command: "sleep 0.1; printf 'survived\\n'".into(),
        background: true,
        timeout_ms: None,
    })
    .await
    .unwrap();
    drop(bash);

    let terminal = registry
        .wait(&["job-0".parse().unwrap()], Some(Duration::from_secs(1)))
        .await
        .unwrap();
    assert_eq!(terminal.snapshots[0].state(), JobState::Completed);
    assert!(
        terminal.snapshots[0]
            .details()
            .render()
            .contains("survived")
    );
}

#[tokio::test]
async fn running_snapshot_exposes_partial_output() {
    let directory = tempfile::tempdir().unwrap();
    let (registry, bash) = service(directory.path());
    bash.bash(BashArgs {
        command: "printf 'partial\\n'; sleep 0.3".into(),
        background: true,
        timeout_ms: None,
    })
    .await
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let snapshot = registry.status(Some("job-0".parse().unwrap())).unwrap()[0].clone();
        if snapshot.details().render().contains("partial") {
            assert_eq!(snapshot.state(), JobState::Running);
            break;
        }
        assert!(Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn large_output_is_tail_truncated_without_stalling_and_keeps_full_log() {
    let directory = tempfile::tempdir().unwrap();
    let (_registry, bash) = service(directory.path());
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        bash.bash(BashArgs {
            command: "for i in $(seq 1 3000); do printf 'line-%04d-abcdefghijklmnopqrstuvwxyz\\n' \"$i\"; done".into(),
            background: false,
            timeout_ms: None,
        }),
    )
    .await
    .unwrap()
    .unwrap();
    let text = output.as_text().unwrap();
    assert!(!text.contains("line-0001-"));
    assert!(text.contains("line-3000-"));
    let path = full_output_path(text);
    assert!(path.exists());
    let complete = std::fs::read_to_string(path).unwrap();
    assert!(complete.contains("line-0001-"));
    assert!(complete.contains("line-3000-"));
}

#[tokio::test]
async fn terminal_eviction_unlinks_log_even_with_an_old_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let (registry, bash) = service(directory.path());
    let first = bash
        .bash(BashArgs {
            command: "head -c 60000 /dev/zero | tr '\\0' x".into(),
            background: false,
            timeout_ms: None,
        })
        .await
        .unwrap();
    let old_snapshot = registry.status(Some("job-0".parse().unwrap())).unwrap()[0].clone();
    let path = full_output_path(first.as_text().unwrap()).to_owned();
    assert!(path.exists());
    for _ in 0..64 {
        bash.bash(BashArgs {
            command: "true".into(),
            background: false,
            timeout_ms: None,
        })
        .await
        .unwrap();
    }
    assert!(!path.exists());
    assert!(!old_snapshot.details().render().contains("Full output:"));
}

#[tokio::test]
async fn shutdown_unlinks_a_retained_terminal_log() {
    let directory = tempfile::tempdir().unwrap();
    let (registry, bash) = service(directory.path());
    let output = bash
        .bash(BashArgs {
            command: "head -c 60000 /dev/zero | tr '\\0' x".into(),
            background: false,
            timeout_ms: None,
        })
        .await
        .unwrap();
    let path = full_output_path(output.as_text().unwrap()).to_owned();
    registry.shutdown().await.unwrap();
    assert!(!path.exists());
    let snapshot = registry.status(Some("job-0".parse().unwrap())).unwrap()[0].clone();
    assert!(!snapshot.details().render().contains("Full output:"));
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_terminates_descendant_processes() {
    use nix::{errno::Errno, sys::signal::kill, unistd::Pid};
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("pid");
    let (registry, bash) = service(directory.path());
    bash.bash(BashArgs {
        command: "sleep 30 & echo $! > pid.tmp; mv pid.tmp pid; wait".into(),
        background: true,
        timeout_ms: None,
    })
    .await
    .unwrap();
    while !pid_file.exists() {
        tokio::task::yield_now().await;
    }
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    registry.cancel("job-0".parse().unwrap()).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if kill(Pid::from_raw(pid), None) == Err(Errno::ESRCH) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "descendant process remained alive"
        );
        tokio::task::yield_now().await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_force_kills_a_sigterm_resistant_descendant() {
    use nix::{errno::Errno, sys::signal::kill, unistd::Pid};
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("pid");
    let (registry, bash) = service(directory.path());
    bash.bash(BashArgs {
        command: "(trap '' TERM; sleep 30) & echo $! > pid.tmp; mv pid.tmp pid; wait".into(),
        background: true,
        timeout_ms: None,
    })
    .await
    .unwrap();
    while !pid_file.exists() {
        tokio::task::yield_now().await;
    }
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    tokio::time::timeout(
        Duration::from_secs(4),
        registry.cancel("job-0".parse().unwrap()),
    )
    .await
    .expect("cancellation should force-kill the resistant process group")
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if kill(Pid::from_raw(pid), None) == Err(Errno::ESRCH) {
            break;
        }
        assert!(Instant::now() < deadline, "resistant descendant survived");
        tokio::task::yield_now().await;
    }
}
