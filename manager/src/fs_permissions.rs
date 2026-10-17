//! Modes of the files and directories the daemon creates.
//!
//! POSIX only: Windows ACLs are a different check. The test sets umask 022
//! and expects private modes (directories 0700, files 0600). A home that
//! already exists as 0777 must be rejected or tightened, not reused as-is.

#[cfg(unix)]
mod unix {
    use crate::events;
    use crate::ipc;
    use crate::lifecycle::{self, Claim};
    use crate::proto::{TaskKind, TaskRecord, TaskStatus};
    use crate::registry::{self, task_output_path};
    use crate::task::open_output_files;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    unsafe extern "C" {
        fn umask(mask: u32) -> u32;
    }

    struct UmaskGuard(u32);

    impl UmaskGuard {
        fn set(mask: u32) -> Self {
            Self(unsafe { umask(mask) })
        }
    }

    impl Drop for UmaskGuard {
        fn drop(&mut self) {
            unsafe {
                umask(self.0);
            }
        }
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pi-famulus-mode-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        dir
    }

    fn record(home: &Path) -> TaskRecord {
        let output = task_output_path(home, "s1", "sh_0000000a");
        TaskRecord {
            task_id: "sh_0000000a".into(),
            session_id: "s1".into(),
            kind: TaskKind::Shell,
            command: "printf hi".into(),
            cwd: "/tmp".into(),
            pid: 1,
            status: TaskStatus::Running,
            exit_code: None,
            signal: None,
            started_at: 1,
            ended_at: None,
            output_path: output.to_string_lossy().into_owned(),
            output_size: 0,
            origin: None,
            backgrounded_at: None,
            end_reason: None,
            cpu_user_ms: None,
            cpu_sys_ms: None,
            max_rss_kb: None,
            live_cpu_user_ms: None,
            live_cpu_sys_ms: None,
            live_cpu_percent: None,
            live_cpu_sampled_at: None,
            live_cpu_stale: false,
            label: None,
        }
    }

    fn expect_mode(violations: &mut Vec<String>, path: &Path, want: u32) {
        match fs::metadata(path) {
            Ok(meta) => {
                let got = meta.permissions().mode() & 0o777;
                if got != want {
                    violations.push(format!("{} is {got:04o}, want {want:04o}", path.display()));
                }
            }
            Err(error) => violations.push(format!("{} missing: {error}", path.display())),
        }
    }

    #[tokio::test]
    async fn created_paths_are_private_under_umask_022() {
        let _umask = UmaskGuard::set(0o022);
        let home = temp_home("fresh");
        let mut violations = Vec::new();
        // daemon::run creates the home with create_dir_all and no mode of its own.
        fs::create_dir_all(&home).unwrap();
        expect_mode(&mut violations, &home, 0o700);

        let guard = match lifecycle::claim_daemon(&home).unwrap() {
            Claim::Acquired(guard) => guard,
            Claim::AlreadyRunning { .. } => panic!("fresh home was already locked"),
        };
        lifecycle::write_pid_file(&home, std::process::id()).unwrap();
        lifecycle::log_line(&home, "mode probe");
        let listener = ipc::bind(&home).await.unwrap();
        registry::persist_record(&home, &record(&home)).unwrap();
        let output = task_output_path(&home, "s1", "sh_0000000a");
        let _files = open_output_files(&output).unwrap();
        events::emit(
            &home,
            Some("s1"),
            "task.start",
            Some("sh_0000000a"),
            serde_json::json!({}),
        );
        events::emit(&home, None, "daemon.start", None, serde_json::json!({}));

        expect_mode(&mut violations, &lifecycle::pid_path(&home), 0o600);
        expect_mode(&mut violations, &lifecycle::log_path(&home), 0o600);
        expect_mode(&mut violations, &lifecycle::daemon_lock_path(&home), 0o600);
        expect_mode(&mut violations, &lifecycle::socket_path(&home), 0o600);
        expect_mode(&mut violations, &registry::tasks_dir(&home, "s1"), 0o700);
        expect_mode(&mut violations, &home.join("sessions"), 0o700);
        expect_mode(&mut violations, &home.join("sessions").join("s1"), 0o700);
        expect_mode(
            &mut violations,
            &registry::task_json_path(&home, "s1", "sh_0000000a"),
            0o600,
        );
        expect_mode(&mut violations, &output, 0o600);
        expect_mode(&mut violations, &crate::task::stderr_path_for(&output), 0o600);
        expect_mode(&mut violations, &events::session_events_path(&home, "s1"), 0o600);
        expect_mode(&mut violations, &events::daemon_events_path(&home), 0o600);

        drop(listener);
        drop(guard);
        let _ = fs::remove_file(lifecycle::socket_path(&home));
        let _ = fs::remove_dir_all(&home);
        assert!(
            violations.is_empty(),
            "created paths are not private:\n{}",
            violations.join("\n")
        );
    }

    #[tokio::test]
    async fn preexisting_world_writable_home_is_rejected_or_fixed() {
        let _umask = UmaskGuard::set(0o022);
        let home = temp_home("open");
        fs::create_dir(&home).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(mode_of(&home), 0o777);

        let claimed = lifecycle::claim_daemon(&home);
        let adopted = match claimed {
            Ok(Claim::Acquired(guard)) => {
                let wrote = lifecycle::write_pid_file(&home, std::process::id());
                let bound = ipc::bind(&home).await;
                drop(bound.ok());
                drop(guard);
                let _ = fs::remove_file(lifecycle::socket_path(&home));
                wrote.is_ok() && mode_of(&home) == 0o777
            }
            Ok(Claim::AlreadyRunning { .. }) => false,
            Err(_) => false,
        };
        let _ = fs::remove_dir_all(&home);
        assert!(
            !adopted,
            "a 0777 home was locked and written without being rejected or tightened to 0700"
        );
    }
}
