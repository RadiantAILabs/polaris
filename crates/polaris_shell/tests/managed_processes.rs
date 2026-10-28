//! Behavioural tests for the managed shell process lifecycle.

use polaris_shell::{
    ChannelSink, ProcessTreeGuarantee, ShellCancel, ShellConfig, ShellEvent, ShellEventReceiver,
    ShellOutcome, ShellPlugin, ShellProcessState, ShellRequest, ShellSinkHandle, ShellStream,
};
use polaris_shell::{ShellExecutor, ShellProcessId};
use polaris_system::server::Server;
use polaris_tools::ToolsPlugin;
use std::num::NonZeroUsize;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::watch;

fn executor() -> ShellExecutor {
    ShellExecutor::new(
        ShellConfig::new()
            .with_allowed_commands(vec!["*".to_owned()])
            .disable_sandbox()
            .disable_cache(),
    )
}

/// Builds a sink bounded to `capacity` data events.
fn channel_sink(capacity: usize) -> (ShellSinkHandle, ShellEventReceiver) {
    let (sink, events) = ChannelSink::new(NonZeroUsize::new(capacity).expect("non-zero capacity"));
    (ShellSinkHandle::new(sink), events)
}

async fn wait_until_finished(executor: &ShellExecutor, id: ShellProcessId) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if executor
                .process(id)
                .is_some_and(|status| matches!(status.state, ShellProcessState::Finished(_)))
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("process should finish");
}

#[tokio::test]
async fn streams_stdout_and_stderr_before_exit_with_ordered_lifecycle() {
    let temp = TempDir::new().unwrap();
    let release = temp.path().join("release");
    let executor = executor();
    let (sink, mut events) = channel_sink(16);
    // The child blocks after its first writes until this test creates the
    // release file, so "a chunk arrives before the process exits" is an
    // ordering assertion rather than a race against wall-clock time.
    let process = executor
        .start_confirmed(
            ShellRequest::new(format!(
                "printf 'stdout-one'; printf 'stderr-one' >&2; \
                 while [ ! -e '{}' ]; do sleep 0.01; done; printf 'stdout-two'",
                release.display()
            ))
            .with_sink(sink),
        )
        .await
        .unwrap();

    assert!(matches!(
        events.recv().await,
        Some(ShellEvent::Started { .. })
    ));
    let first_chunk = events
        .recv()
        .await
        .expect("incremental output should arrive before process exit");
    assert!(
        matches!(first_chunk, ShellEvent::Chunk { .. }),
        "expected a chunk before the process could exit, got: {first_chunk:?}"
    );
    assert_eq!(
        executor.process(process.id()).unwrap().state,
        ShellProcessState::Running,
        "the child must still be running when the first chunk arrives"
    );

    std::fs::write(&release, b"go").unwrap();
    let result = process.wait().await.unwrap().unwrap();
    assert_eq!(result.outcome, ShellOutcome::Exited);
    assert_eq!(result.stdout, "stdout-onestdout-two");
    assert_eq!(result.stderr, "stderr-one");

    let mut seen = vec![first_chunk];
    while let Some(event) = events.recv().await {
        seen.push(event);
    }
    let saw_stdout = seen.iter().any(
        |event| matches!(event, ShellEvent::Chunk { stream, .. } if *stream == ShellStream::Stdout),
    );
    let saw_stderr = seen.iter().any(
        |event| matches!(event, ShellEvent::Chunk { stream, .. } if *stream == ShellStream::Stderr),
    );
    assert!(saw_stdout, "expected at least one stdout chunk: {seen:?}");
    assert!(saw_stderr, "expected at least one stderr chunk: {seen:?}");
    assert!(
        matches!(seen.last(), Some(ShellEvent::Finished { .. })),
        "Finished must be the last event delivered: {seen:?}"
    );
    assert_eq!(
        seen.iter()
            .filter(|event| matches!(event, ShellEvent::Finished { .. }))
            .count(),
        1,
        "Finished must be delivered exactly once: {seen:?}"
    );
}

#[tokio::test]
async fn full_channel_retains_lifecycle_and_reports_drops() {
    let temp = TempDir::new().unwrap();
    let release = temp.path().join("release");
    let executor = executor();
    let (sink, mut events) = channel_sink(1);
    let process = executor
        .start_confirmed(
            ShellRequest::new(format!(
                "printf x; while [ ! -e '{}' ]; do sleep 0.01; done; printf y",
                release.display()
            ))
            .with_sink(sink),
        )
        .await
        .unwrap();
    // Observe the first completed reader poll before allowing the second
    // write; the one-slot queue stays full throughout this test.
    tokio::time::timeout(Duration::from_secs(5), async {
        while executor.process(process.id()).unwrap().stdout_bytes < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    std::fs::write(release, b"go").unwrap();
    let result = process.wait().await.unwrap().unwrap();
    assert!(
        result.dropped_events > 0,
        "the second gated write must encounter the full queue"
    );
    // The point of the reserved lifecycle slots: dropping sink events must
    // never cost captured output.
    assert_eq!(
        result.stdout, "xy",
        "every byte must still be captured despite the dropped events"
    );
    assert!(matches!(
        events.recv().await,
        Some(ShellEvent::Started { .. })
    ));
    assert!(matches!(
        events.recv().await,
        Some(ShellEvent::Chunk { .. })
    ));
    assert!(matches!(
        events.recv().await,
        Some(ShellEvent::Finished { .. })
    ));
    assert!(events.recv().await.is_none());
}

#[tokio::test]
async fn timed_out_run_still_ends_with_finished_last() {
    // Forces the drain-deadline branch that aborts the readers: a chunk read
    // concurrently with the abort must not be delivered after `Finished`.
    let executor = ShellExecutor::new(
        ShellConfig::new()
            .with_allowed_commands(vec!["*".to_owned()])
            .disable_sandbox()
            .disable_cache()
            .with_terminate_grace(Duration::from_millis(20))
            .with_drain_deadline(Duration::from_millis(1)),
    );
    let (sink, mut events) = channel_sink(64);
    let process = executor
        .start_confirmed(
            ShellRequest::new("while :; do printf chatter; done")
                .with_timeout(1)
                .with_sink(sink),
        )
        .await
        .unwrap();
    let result = process.wait().await.unwrap().unwrap();
    assert_eq!(result.outcome, ShellOutcome::TimedOut);

    let mut seen = Vec::new();
    while let Some(event) = events.recv().await {
        seen.push(event);
    }
    assert!(
        matches!(seen.first(), Some(ShellEvent::Started { .. })),
        "Started must be the first event: {:?}",
        seen.first()
    );
    assert!(
        matches!(seen.last(), Some(ShellEvent::Finished { .. })),
        "Finished must be the last event even when readers are aborted: {:?}",
        seen.last()
    );
    let finished_at = seen
        .iter()
        .position(|event| matches!(event, ShellEvent::Finished { .. }))
        .expect("Finished should be delivered");
    assert_eq!(
        finished_at,
        seen.len() - 1,
        "no event may follow Finished: {seen:?}"
    );
}

#[tokio::test]
async fn pre_spawn_cancellation_has_identity_without_os_process() {
    let executor = executor();
    let temp = TempDir::new().unwrap();
    let marker = temp.path().join("should-not-exist");
    let (_source, receiver) = watch::channel(true);
    let (sink, mut events) = channel_sink(1);
    let process = executor
        .start_confirmed(
            ShellRequest::new("touch should-not-exist")
                .with_working_dir(temp.path())
                .with_cancel(ShellCancel::from_watch(receiver))
                .with_sink(sink),
        )
        .await
        .unwrap();
    assert!(process.os_pid().is_none());
    assert!(matches!(
        events.recv().await,
        Some(ShellEvent::Started { os_pid: None, .. })
    ));
    assert!(matches!(
        events.recv().await,
        Some(ShellEvent::Finished {
            outcome: ShellOutcome::Cancelled,
            ..
        })
    ));
    let result = process.wait().await.unwrap().unwrap();
    assert_eq!(result.outcome, ShellOutcome::Cancelled);
    assert!(result.cleanup_completed);
    assert!(
        !marker.exists(),
        "pre-cancelled commands must have no side effects"
    );
}

#[tokio::test]
async fn terminal_result_is_consumed_exactly_once() {
    let executor = executor();
    let process = executor
        .start_confirmed(ShellRequest::new("printf done"))
        .await
        .unwrap();
    let id = process.id();
    let (from_handle, from_executor) = tokio::join!(process.wait(), executor.wait_for(id));
    assert_ne!(
        from_handle.is_some(),
        from_executor.is_some(),
        "exactly one of the two waiters must receive the result"
    );
    assert!(executor.wait_for(id).await.is_none());
}

#[tokio::test]
async fn a_foreground_run_is_reserved_and_wait_for_refuses_it() {
    // `execute` must win unconditionally: an outside waiter that finds the run
    // through `processes()` observes it as not waitable rather than racing for
    // the result.
    let temp = TempDir::new().unwrap();
    let release = temp.path().join("release");
    let watcher_release = release.clone();
    let executor = executor();
    let observer = executor.clone();
    let watcher = tokio::spawn(async move {
        let status = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(status) = observer.processes().into_iter().next() {
                    return status;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the foreground run should be discoverable while it is live");
        let stolen = observer.wait_for(status.id).await;
        std::fs::write(watcher_release, b"go").unwrap();
        (status.reserved, stolen)
    });

    let result = executor
        .execute_confirmed(ShellRequest::new(format!(
            "while [ ! -e '{}' ]; do sleep 0.01; done; printf foreground",
            release.display()
        )))
        .await
        .expect("the foreground caller always receives its own result");
    assert_eq!(result.stdout, "foreground");

    let (reserved, stolen) = watcher.await.expect("watcher should not panic");
    assert!(reserved, "a foreground run must report itself reserved");
    assert!(
        stolen.is_none(),
        "wait_for must refuse a reserved run, got: {stolen:?}"
    );
}

#[tokio::test]
async fn completed_result_retention_evicts_oldest_entry() {
    let executor = ShellExecutor::new(
        ShellConfig::new()
            .with_allowed_commands(vec!["*".to_owned()])
            .disable_sandbox()
            .disable_cache()
            .with_retained_completed_results(2),
    );
    let mut ids = Vec::new();
    for _ in 0..3 {
        let process = executor
            .start_confirmed(ShellRequest::new("true"))
            .await
            .unwrap();
        ids.push(process.detach());
        wait_until_finished(&executor, *ids.last().unwrap()).await;
    }
    assert!(executor.process(ids[0]).is_none());
    assert!(executor.process(ids[1]).is_some());
    assert!(executor.process(ids[2]).is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_spawns_in_the_validated_canonical_directory() {
    let temp = TempDir::new().unwrap();
    let real = temp.path().join("real");
    let alias = temp.path().join("alias");
    std::fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let canonical = real.canonicalize().unwrap();
    let executor = ShellExecutor::new(
        ShellConfig::new()
            .with_allowed_commands(vec!["pwd".to_owned()])
            .with_allowed_dirs(vec![real])
            .disable_cache(),
    );

    let process = executor
        .start_confirmed(ShellRequest::new("pwd").with_working_dir(alias))
        .await
        .unwrap();
    let status = executor.process(process.id()).unwrap();
    assert_eq!(
        status.working_dir, canonical,
        "the process table must retain the checked path, not the mutable alias"
    );

    let result = process.wait().await.unwrap().unwrap();
    assert_eq!(result.stdout.trim(), canonical.to_string_lossy());
}

#[tokio::test]
async fn overflow_file_contains_complete_stream() {
    let cache = TempDir::new().unwrap();
    let executor = ShellExecutor::new(
        ShellConfig::new()
            .with_allowed_commands(vec!["*".to_owned()])
            .disable_sandbox()
            .with_cache_dir(cache.path())
            .with_max_output_bytes(4),
    );
    let result = executor
        .execute_confirmed(ShellRequest::new("printf 0123456789"))
        .await
        .unwrap();
    assert_eq!(result.stdout, "0123\n...[truncated, 4 byte limit]");
    assert!(!result.stdout_overflow_truncated);
    let overflow = result.stdout_overflow_path.unwrap();
    assert_eq!(tokio::fs::read(overflow).await.unwrap(), b"0123456789");
}

#[tokio::test]
async fn stderr_overflow_is_captured_independently_of_stdout() {
    let cache = TempDir::new().unwrap();
    let executor = ShellExecutor::new(
        ShellConfig::new()
            .with_allowed_commands(vec!["*".to_owned()])
            .disable_sandbox()
            .with_cache_dir(cache.path())
            .with_max_output_bytes(4),
    );
    let result = executor
        .execute_confirmed(ShellRequest::new("printf ab; printf 0123456789 >&2"))
        .await
        .unwrap();
    assert!(!result.stdout_truncated, "stdout fits under the limit");
    assert_eq!(result.stdout, "ab");
    assert!(result.stderr_truncated);
    assert!(result.stdout_overflow_path.is_none());
    let overflow = result
        .stderr_overflow_path
        .expect("stderr should have its own overflow file");
    assert_eq!(tokio::fs::read(overflow).await.unwrap(), b"0123456789");
}

#[tokio::test]
async fn overflow_spill_stops_at_the_configured_quota() {
    let cache = TempDir::new().unwrap();
    let executor = ShellExecutor::new(
        ShellConfig::new()
            .with_allowed_commands(vec!["*".to_owned()])
            .disable_sandbox()
            .with_cache_dir(cache.path())
            .with_max_output_bytes(4)
            .with_max_overflow_bytes(8),
    );
    let (sink, mut events) = channel_sink(64);
    let process = executor
        .start_confirmed(ShellRequest::new("printf 0123456789abcdef").with_sink(sink))
        .await
        .unwrap();
    let result = process.wait().await.unwrap().unwrap();

    assert!(result.stdout_truncated);
    assert!(
        result.stdout_overflow_truncated,
        "the spill quota must be reported in the result"
    );
    let overflow = result
        .stdout_overflow_path
        .expect("a partial overflow file is still offered");
    let spilled = tokio::fs::read(overflow).await.unwrap();
    assert_eq!(
        spilled.len(),
        8,
        "no more than the quota may be written to disk"
    );
    assert_eq!(spilled, b"01234567");

    let mut seen = Vec::new();
    while let Some(event) = events.recv().await {
        seen.push(event);
    }
    assert_eq!(
        seen.iter()
            .filter(|event| matches!(event, ShellEvent::OverflowLimitReached { .. }))
            .count(),
        1,
        "the quota must be announced exactly once: {seen:?}"
    );
    assert!(
        seen.iter()
            .any(|event| matches!(event, ShellEvent::CaptureLimitReached { .. })),
        "the in-memory limit is announced too: {seen:?}"
    );
}

#[tokio::test]
async fn stop_all_counts_only_the_transitions_it_won() {
    let executor = ShellExecutor::new(
        ShellConfig::new()
            .with_allowed_commands(vec!["*".to_owned()])
            .disable_sandbox()
            .disable_cache()
            .with_terminate_grace(Duration::from_millis(30)),
    );
    let mut ids = Vec::new();
    for _ in 0..3 {
        let process = executor
            .start_confirmed(ShellRequest::new("sleep 30"))
            .await
            .unwrap();
        ids.push(process.detach());
    }
    // One is already terminating, so `stop_all` can only win the other two.
    assert!(executor.stop(ids[0]).await);

    assert_eq!(executor.stop_all().await, 2);
    for id in ids {
        let status = executor.process(id).expect("entries are retained");
        assert!(
            matches!(status.state, ShellProcessState::Finished(_)),
            "stop_all must not return before every run is terminal: {status:?}"
        );
        let result = executor.wait_for(id).await.unwrap().unwrap();
        assert_eq!(result.outcome, ShellOutcome::Cancelled);
    }
    assert_eq!(
        executor.stop_all().await,
        0,
        "nothing is left to transition"
    );
}

#[tokio::test]
async fn an_owned_cancellation_future_stops_a_running_child() {
    // The `ShellCancel::from_fn` shape a host uses to bridge its own latched
    // cancellation signal. Each observation owns a cloned receiver, like
    // `RunCancellation::cancelled_owned` does.
    let executor = executor();
    let (signal, observed) = watch::channel(false);
    let process = executor
        .start_confirmed(
            ShellRequest::new("sleep 30").with_cancel(ShellCancel::from_fn(move || {
                let mut observed = observed.clone();
                async move {
                    loop {
                        if *observed.borrow_and_update() {
                            return;
                        }
                        if observed.changed().await.is_err() {
                            return;
                        }
                    }
                }
            })),
        )
        .await
        .unwrap();
    let id = process.id();
    assert_eq!(
        executor.process(id).unwrap().state,
        ShellProcessState::Running
    );

    signal.send_replace(true);

    let result = process.wait().await.unwrap().unwrap();
    assert_eq!(result.outcome, ShellOutcome::Cancelled);
    assert!(result.cleanup_completed);
}

#[tokio::test]
async fn an_already_raised_owned_cancellation_prevents_the_spawn() {
    let executor = executor();
    let temp = TempDir::new().unwrap();
    let process = executor
        .start_confirmed(
            ShellRequest::new("touch should-not-exist")
                .with_working_dir(temp.path())
                .with_cancel(ShellCancel::from_fn(|| std::future::ready(()))),
        )
        .await
        .unwrap();
    assert!(
        process.os_pid().is_none(),
        "a factory that resolves on the first poll must be caught before spawning"
    );
    let result = process.wait().await.unwrap().unwrap();
    assert_eq!(result.outcome, ShellOutcome::Cancelled);
    assert!(!temp.path().join("should-not-exist").exists());
}

#[tokio::test]
async fn timeout_returns_partial_output_and_truthful_cleanup() {
    let executor = ShellExecutor::new(
        ShellConfig::new()
            .with_allowed_commands(vec!["*".to_owned()])
            .disable_sandbox()
            .disable_cache()
            .with_terminate_grace(Duration::from_millis(30)),
    );
    let result = executor
        .execute_confirmed(ShellRequest::new("printf partial; sleep 30").with_timeout(1))
        .await
        .unwrap();
    assert_eq!(result.outcome, ShellOutcome::TimedOut);
    assert!(result.timed_out);
    assert!(result.stdout.contains("partial"));
    assert!(result.stderr.contains("timed out"));
    assert!(result.cleanup_completed);
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_reports_terminating_and_kills_descendant_group() {
    let temp = TempDir::new().unwrap();
    let child_pid_file = temp.path().join("child.pid");
    let command = format!(
        "trap '' TERM; sh -c 'trap \"\" TERM; i=0; while [ $i -lt 100 ]; do sleep 0.1; i=$((i+1)); done' & {}; \
         i=0; while [ $i -lt 100 ]; do sleep 0.1; i=$((i+1)); done",
        write_pid_atomically("$!", &child_pid_file)
    );
    let executor = ShellExecutor::new(
        ShellConfig::new()
            .with_allowed_commands(vec!["*".to_owned()])
            .disable_sandbox()
            .disable_cache()
            .with_terminate_grace(Duration::from_millis(150)),
    );
    assert_eq!(ShellExecutor::tree_guarantee(), ProcessTreeGuarantee::Group);
    let process = executor
        .start_confirmed(ShellRequest::new(command))
        .await
        .unwrap();
    let id = process.id();
    let child_pid = read_pid(&child_pid_file).await;
    assert!(process.stop().await);
    assert_eq!(
        executor.process(id).unwrap().state,
        ShellProcessState::Terminating
    );
    assert!(!process.stop().await);
    let result = process.wait().await.unwrap().unwrap();
    assert_eq!(result.outcome, ShellOutcome::Cancelled);
    assert!(result.cleanup_completed);
    wait_until_process_gone(child_pid).await;
    assert!(!executor.stop(id).await);
}

#[cfg(unix)]
#[tokio::test]
async fn plugin_cleanup_reaps_detached_process_and_its_descendant() {
    let temp = TempDir::new().unwrap();
    let child_pid_file = temp.path().join("cleanup-child.pid");
    let command = format!(
        "sleep 30 & {}; wait",
        write_pid_atomically("$!", &child_pid_file)
    );
    let mut server = Server::new();
    server.add_plugins(polaris_app::AppPlugin::new(
        polaris_app::AppConfig::new().with_host("127.0.0.1"),
    ));
    server.add_plugins(ToolsPlugin);
    server.add_plugins(ShellPlugin::new(
        ShellConfig::new()
            .with_allowed_commands(vec!["*".to_owned()])
            .disable_sandbox()
            .disable_cache()
            .with_terminate_grace(Duration::from_millis(30)),
    ));
    server.finish().await.unwrap();
    let executor = (*server.get_global::<ShellExecutor>().unwrap()).clone();
    let process = executor
        .start_confirmed(ShellRequest::new(command))
        .await
        .unwrap();
    // `detach` — not `drop` — is what hands a run to the executor; dropping
    // would stop it here and never exercise the shutdown backstop.
    let id = process.detach();
    let child_pid = read_pid(&child_pid_file).await;

    server.cleanup().await;

    let result = executor.wait_for(id).await.unwrap().unwrap();
    assert_eq!(result.outcome, ShellOutcome::Cancelled);
    assert!(result.cleanup_completed);
    wait_until_process_gone(child_pid).await;
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_the_handle_stops_the_child() {
    let temp = TempDir::new().unwrap();
    let child_pid_file = temp.path().join("dropped.pid");
    let executor = ShellExecutor::new(
        ShellConfig::new()
            .with_allowed_commands(vec!["*".to_owned()])
            .disable_sandbox()
            .disable_cache()
            .with_terminate_grace(Duration::from_millis(30)),
    );
    let process = executor
        .start_confirmed(ShellRequest::new(format!(
            "{}; sleep 30",
            write_pid_atomically("$$", &child_pid_file)
        )))
        .await
        .unwrap();
    let id = process.id();
    let child_pid = read_pid(&child_pid_file).await;
    assert!(process_exists(child_pid), "the child should be running");

    drop(process);

    wait_until_finished(&executor, id).await;
    let result = executor.wait_for(id).await.unwrap().unwrap();
    assert_eq!(result.outcome, ShellOutcome::Cancelled);
    wait_until_process_gone(child_pid).await;
}

#[cfg(unix)]
#[tokio::test]
async fn detaching_the_handle_leaves_the_child_running() {
    let temp = TempDir::new().unwrap();
    let child_pid_file = temp.path().join("detached.pid");
    let executor = ShellExecutor::new(
        ShellConfig::new()
            .with_allowed_commands(vec!["*".to_owned()])
            .disable_sandbox()
            .disable_cache()
            .with_terminate_grace(Duration::from_millis(30)),
    );
    let process = executor
        .start_confirmed(ShellRequest::new(format!(
            "{}; sleep 30",
            write_pid_atomically("$$", &child_pid_file)
        )))
        .await
        .unwrap();
    let child_pid = read_pid(&child_pid_file).await;
    let id = process.detach();

    // A stop-on-drop bug changes the state synchronously during detach.
    assert_eq!(
        executor.process(id).unwrap().state,
        ShellProcessState::Running,
        "a detached run must keep running"
    );
    assert!(process_exists(child_pid), "detach must not stop the child");

    assert!(executor.stop(id).await);
    let result = executor.wait_for(id).await.unwrap().unwrap();
    assert_eq!(result.outcome, ShellOutcome::Cancelled);
}

/// Keeps the group leader unreaped until after the delayed group `SIGKILL`.
///
/// The process-group id is the leader's pid, so reaping the leader as soon as
/// it exits would free the number and let the later `kill(-pgid, SIGKILL)`
/// reach an unrelated group. The observable consequence is that the leader's
/// `/proc` entry must still exist — as a zombie — part-way through the grace
/// period, even though the leader itself dies on the first `SIGTERM`.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn group_leader_is_not_reaped_before_the_delayed_group_kill() {
    let temp = TempDir::new().unwrap();
    let child_pid_file = temp.path().join("survivor.pid");
    let grace = Duration::from_secs(5);
    let command = format!(
        "sh -c 'trap \"\" TERM; i=0; while [ $i -lt 100 ]; do sleep 0.1; i=$((i+1)); done' & {}; i=0; while [ $i -lt 100 ]; do sleep 0.1; i=$((i+1)); done",
        write_pid_atomically("$!", &child_pid_file)
    );
    let executor = ShellExecutor::new(
        ShellConfig::new()
            .with_allowed_commands(vec!["*".to_owned()])
            .disable_sandbox()
            .disable_cache()
            .with_terminate_grace(grace),
    );
    let process = executor
        .start_confirmed(ShellRequest::new(command))
        .await
        .unwrap();
    let leader_pid = i32::try_from(process.os_pid().expect("a child was spawned")).unwrap();
    let descendant_pid = read_pid(&child_pid_file).await;

    assert!(process.stop().await);
    // The leader takes the SIGTERM immediately; only the grace period keeps it
    // around, and only because it is deliberately left unreaped.
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if std::fs::read_to_string(format!("/proc/{leader_pid}/stat")).is_ok_and(|stat| {
                stat.rsplit_once(") ")
                    .is_some_and(|(_, fields)| fields.starts_with('Z'))
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the terminated leader should become a zombie before group cleanup");
    assert!(
        proc_entry_exists(leader_pid),
        "the leader must stay unreaped until the group SIGKILL is sent"
    );

    let result = process.wait().await.unwrap().unwrap();
    assert_eq!(result.outcome, ShellOutcome::Cancelled);
    assert!(result.cleanup_completed, "the leader must then be reaped");
    assert!(
        !proc_entry_exists(leader_pid),
        "the leader must not be left as a zombie"
    );
    wait_until_process_gone(descendant_pid).await;
}

/// Builds a shell fragment that publishes `expr` to `path` without ever
/// leaving a readable-but-empty file behind.
///
/// `echo $! > file` truncates the file before writing it, so a reader watching
/// for the file's existence can observe an empty string.
#[cfg(unix)]
fn write_pid_atomically(expr: &str, path: &Path) -> String {
    let path = path.display();
    format!("echo {expr} > '{path}.tmp' && mv '{path}.tmp' '{path}'")
}

/// Waits for a pid file written by [`write_pid_atomically`] and parses it.
#[cfg(unix)]
async fn read_pid(path: &Path) -> i32 {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(contents) = tokio::fs::read_to_string(path).await
                && let Ok(pid) = contents.trim().parse::<i32>()
            {
                return pid;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fixture should write a parseable pid file")
}

/// Reports whether the kernel still has a slot for `pid`, zombie or not.
#[cfg(target_os = "linux")]
fn proc_entry_exists(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(unix)]
async fn wait_until_process_gone(pid: i32) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while process_exists(pid) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the signalled fixture process must exit");
}

#[cfg(unix)]
fn process_exists(pid: i32) -> bool {
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        && stat
            .rsplit_once(") ")
            .and_then(|(_, fields)| fields.chars().next())
            == Some('Z')
    {
        return false;
    }
    // SAFETY: signal zero performs existence/permission checking only and does
    // not dereference memory or change process state.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn replacing_a_validated_directory_does_not_redirect_spawn() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let temp = TempDir::new().unwrap();
    let allowed = temp.path().join("allowed");
    let leaf = allowed.join("leaf");
    let retained = allowed.join("retained");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&leaf).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let swapped = Arc::new(AtomicBool::new(false));
    let cancel = ShellCancel::from_fn({
        let leaf = leaf.clone();
        let retained = retained.clone();
        move || {
            if !swapped.swap(true, Ordering::SeqCst) {
                std::fs::rename(&leaf, &retained).unwrap();
                std::os::unix::fs::symlink(&outside, &leaf).unwrap();
            }
            std::future::pending::<()>()
        }
    });
    let executor = ShellExecutor::new(
        ShellConfig::new()
            .with_allowed_dirs(vec![allowed])
            .disable_cache(),
    );
    let result = executor
        .execute_confirmed(
            ShellRequest::new("pwd -P")
                .with_working_dir(leaf)
                .with_cancel(cancel),
        )
        .await
        .unwrap();
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(
        result.stdout.trim(),
        retained.canonicalize().unwrap().to_string_lossy()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn normal_leader_exit_terminates_background_group_members() {
    let temp = TempDir::new().unwrap();
    let pid_file = temp.path().join("background.pid");
    let release = temp.path().join("release");
    let process = executor()
        .start_confirmed(ShellRequest::new(format!(
            "sleep 30 >/dev/null 2>&1 & {}; while [ ! -e '{}' ]; do sleep 0.01; done",
            write_pid_atomically("$!", &pid_file),
            release.display(),
        )))
        .await
        .unwrap();
    let child_pid = read_pid(&pid_file).await;
    #[cfg(target_os = "linux")]
    let _guard = PidGuard::new(child_pid);
    std::fs::write(release, b"go").unwrap();
    let result = process.wait().await.unwrap().unwrap();
    assert_eq!(result.outcome, ShellOutcome::Exited);
    assert!(result.cleanup_completed);
    tokio::time::timeout(Duration::from_secs(5), async {
        while process_exists(child_pid) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a normal shell exit must clean up its process group");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn escaped_descendant_holding_a_pipe_reports_incomplete_cleanup() {
    let temp = TempDir::new().unwrap();
    let pid_file = temp.path().join("escaped.pid");
    let command = format!(
        "setsid sh -c '{}; exec sleep 30' & while [ ! -e '{}' ]; do sleep 0.01; done",
        write_pid_atomically("$$", &pid_file).replace('\'', "\""),
        pid_file.display(),
    );
    let executor = ShellExecutor::new(
        ShellConfig::new()
            .disable_sandbox()
            .disable_cache()
            .with_drain_deadline(Duration::from_millis(100)),
    );
    let process = executor
        .start_confirmed(ShellRequest::new(command))
        .await
        .unwrap();
    let escaped_pid = read_pid(&pid_file).await;
    let _guard = PidGuard::new(escaped_pid);
    let result = tokio::time::timeout(Duration::from_secs(5), process.wait())
        .await
        .expect("an escaped descendant must not hold completion open indefinitely")
        .unwrap()
        .unwrap();
    assert_eq!(result.outcome, ShellOutcome::Exited);
    assert!(
        !result.cleanup_completed,
        "an open pipe prevents complete cleanup"
    );
    assert!(
        process_exists(escaped_pid),
        "the process left the supervised group"
    );
}

/// Owns a kernel reference so panic cleanup cannot signal a recycled pid.
#[cfg(target_os = "linux")]
struct PidGuard(std::os::fd::OwnedFd);

#[cfg(target_os = "linux")]
impl PidGuard {
    fn new(pid: i32) -> Self {
        use std::os::fd::FromRawFd;
        // SAFETY: pidfd_open takes a process id and zero flags, with no pointers.
        let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        let descriptor = i32::try_from(descriptor).unwrap();
        assert!(
            descriptor >= 0,
            "pidfd_open: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: successful pidfd_open returns a fresh, owned descriptor.
        Self(unsafe { std::os::fd::OwnedFd::from_raw_fd(descriptor) })
    }
}

#[cfg(target_os = "linux")]
impl Drop for PidGuard {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // SAFETY: the owned pidfd identifies the fixture process; a null
        // siginfo pointer requests an ordinary signal with zero flags.
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            );
        }
    }
}

#[tokio::test]
async fn gated_output_preserves_capture_boundaries_and_later_spill_reads() {
    for total in [3_usize, 4, 5, 7] {
        let temp = TempDir::new().unwrap();
        let release = temp.path().join("release");
        let cache = temp.path().join("cache");
        let executor = ShellExecutor::new(
            ShellConfig::new()
                .disable_sandbox()
                .with_cache_dir(&cache)
                .with_max_output_bytes(4),
        );
        let (sink, mut events) = channel_sink(16);
        let first_overflow = if total > 4 { "4" } else { "" };
        let rest = &"56"[..total.saturating_sub(5)];
        let release_later = temp.path().join("release-later");
        let prefix = &"0123"[..total.min(4)];
        let process = executor.start_confirmed(
            ShellRequest::new(format!(
                "printf '{prefix}'; while [ ! -e '{}' ]; do sleep 0.01; done; printf '{first_overflow}'; while [ ! -e '{}' ]; do sleep 0.01; done; printf '{rest}'", release.display(), release_later.display()
            )).with_sink(sink),
        ).await.unwrap();
        assert!(matches!(
            events.recv().await,
            Some(ShellEvent::Started { .. })
        ));
        assert!(matches!(
            events.recv().await,
            Some(ShellEvent::Chunk { .. })
        ));
        std::fs::write(&release, b"go").unwrap();
        if total > 4 {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match events
                        .recv()
                        .await
                        .expect("stream closed before the capture limit event")
                    {
                        ShellEvent::CaptureLimitReached { .. } => break,
                        ShellEvent::Finished { .. } => {
                            panic!("run finished before the capture limit event")
                        }
                        _ => {}
                    }
                }
            })
            .await
            .expect("the first overflow byte must cross the limit");
        }
        std::fs::write(&release_later, b"go").unwrap();
        let result = process.wait().await.unwrap().unwrap();
        assert_eq!(result.stdout_truncated, total > 4);
        if total <= 4 {
            assert_eq!(result.stdout, prefix);
            assert!(result.stdout_overflow_path.is_none());
        } else {
            assert_eq!(result.stdout, "0123\n...[truncated, 4 byte limit]");
            let path = result.stdout_overflow_path.unwrap();
            assert_eq!(std::fs::read(path).unwrap(), &b"0123456"[..total]);
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn plugin_cleanup_sweeps_expired_spills_even_after_shutdown_grace_expires() {
    let temp = TempDir::new().unwrap();
    let cache = temp.path().join("cache");
    let mut server = Server::new();
    server.add_plugins(polaris_app::AppPlugin::new(
        polaris_app::AppConfig::new().with_host("127.0.0.1"),
    ));
    server.add_plugins(ToolsPlugin);
    server.add_plugins(ShellPlugin::new(
        ShellConfig::new()
            .disable_sandbox()
            .with_cache_dir(&cache)
            .with_max_output_bytes(4)
            .with_overflow_ttl_secs(60)
            .with_terminate_grace(Duration::from_millis(100))
            .with_shutdown_grace(Duration::ZERO),
    ));
    server.finish().await.unwrap();
    let executor = (*server.get_global::<ShellExecutor>().unwrap()).clone();
    let result = executor
        .execute_confirmed(ShellRequest::new("printf 0123456789; printf abcdef >&2"))
        .await
        .unwrap();
    let stdout = result.stdout_overflow_path.unwrap();
    let stderr = result.stderr_overflow_path.unwrap();
    let old = std::time::SystemTime::now() - Duration::from_secs(120);
    for path in [&stdout, &stderr] {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
    }
    let unrelated = cache.join("unrelated.txt");
    std::fs::write(&unrelated, b"keep").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&unrelated)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(old))
        .unwrap();
    let process = executor
        .start_confirmed(ShellRequest::new("sleep 30"))
        .await
        .unwrap();
    let id = process.detach();
    tokio::time::timeout(Duration::from_secs(5), server.cleanup())
        .await
        .unwrap();
    // A zero shutdown grace expires while the independently owned supervisor
    // continues cleanup. Observe its eventual terminal result explicitly.
    let result = tokio::time::timeout(Duration::from_secs(5), executor.wait_for(id))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result.outcome, ShellOutcome::Cancelled);
    assert!(result.cleanup_completed);
    assert!(!stdout.exists());
    assert!(!stderr.exists());
    assert!(unrelated.exists());
}

#[tokio::test]
async fn a_closed_watch_cancels_before_spawn_without_side_effects() {
    let temp = TempDir::new().unwrap();
    let (sender, observer) = watch::channel(false);
    drop(sender);
    let process = executor()
        .start_confirmed(
            ShellRequest::new("touch marker")
                .with_working_dir(temp.path())
                .with_cancel(ShellCancel::from_watch(observer)),
        )
        .await
        .unwrap();
    assert!(process.os_pid().is_none());
    let result = process.wait().await.unwrap().unwrap();
    assert_eq!(result.outcome, ShellOutcome::Cancelled);
    assert!(!temp.path().join("marker").exists());
}
