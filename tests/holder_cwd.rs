//! Behavioral proof that `harness grok hold --epoch` starts in that worktree.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use coordinator::config::ENV_COORDINATOR_HOME;
use coordinator::registry::{ProjectAddOptions, Registry};
use coordinator::state::{RunEnvelope, RunState, RunStatus, ensure_state_dir, write_run_book};

#[test]
fn holder_child_cwd_is_the_worktree() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mock_dir = tempfile::tempdir().unwrap();
    let mock = mock_dir.path().join("cwd_mock.exe");
    let rustc = Command::new("rustc")
        .arg("tests/cwd_mock.rs")
        .arg("-o")
        .arg(&mock)
        .status()
        .expect("rustc");
    assert!(rustc.success(), "rustc cwd_mock");

    unsafe {
        std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        std::env::set_var("COORDINATOR_WORKTREE", "on");
    }
    let mut reg = Registry::default();
    let rec = reg
        .add(workspace.path(), ProjectAddOptions::default())
        .unwrap();
    reg.projects[0].worktree_isolation = true;
    reg.save(&coordinator::config::registry_path().unwrap())
        .unwrap();
    ensure_state_dir(&rec).unwrap();
    let mut a = RunState::idle(&rec.id);
    a.run_epoch = 3;
    a.status = RunStatus::Running;
    a.phase = "plan".into();
    a.track_id = Some("A".into());
    let mut b = RunState::idle(&rec.id);
    b.run_epoch = 4;
    b.status = RunStatus::Running;
    b.phase = "implement".into();
    b.track_id = Some("B".into());
    write_run_book(&rec, &RunEnvelope::new(0, vec![a, b])).unwrap();

    let root = coordinator::worktree::worktrees_root(&rec).unwrap();
    let dir3 = root.join("3");
    let dir4 = root.join("4");
    std::fs::create_dir_all(&dir3).unwrap();
    std::fs::create_dir_all(&dir4).unwrap();

    let out3 = mock_dir.path().join("cwd-3.txt");
    let out4 = mock_dir.path().join("cwd-4.txt");
    let pid3 = mock_dir.path().join("pid-3.txt");
    let pid4 = mock_dir.path().join("pid-4.txt");
    let bin = env!("CARGO_BIN_EXE_coordinator");
    let mut first = spawn_hold(bin, &rec.id, 3, &mock, &out3, &pid3, home.path());
    let mut second = spawn_hold(bin, &rec.id, 4, &mock, &out4, &pid4, home.path());
    let saw3 = wait_file(&out3);
    let saw4 = wait_file(&out4);
    let _ = first.kill();
    let _ = second.kill();
    let _ = first.wait();
    let _ = second.wait();
    kill_pidfile(&pid3);
    kill_pidfile(&pid4);
    let text3 = saw3.expect("epoch 3 cwd");
    let text4 = saw4.expect("epoch 4 cwd");
    assert!(
        text3.replace('\\', "/").ends_with("/worktrees/3"),
        "{text3}"
    );
    assert!(
        text4.replace('\\', "/").ends_with("/worktrees/4"),
        "{text4}"
    );
    assert_ne!(text3, text4);
}

fn spawn_hold(
    bin: &str,
    project: &str,
    epoch: u64,
    mock: &std::path::Path,
    cwd_out: &std::path::Path,
    pid_out: &std::path::Path,
    home: &std::path::Path,
) -> std::process::Child {
    Command::new(bin)
        .arg("harness")
        .arg("grok")
        .arg("hold")
        .arg("--project")
        .arg(project)
        .arg("--epoch")
        .arg(epoch.to_string())
        .env(ENV_COORDINATOR_HOME, home)
        .env("COORDINATOR_WORKTREE", "on")
        .env("COORDINATOR_GROK_BIN", mock)
        .env("COORDINATOR_CWD_OUT", cwd_out)
        .env("COORDINATOR_CWD_PID", pid_out)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn holder")
}

fn wait_file(path: &std::path::Path) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path)
            && !text.is_empty()
        {
            return Some(text);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn kill_pidfile(path: &PathBuf) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(pid) = text.trim().parse::<u32>() else {
        return;
    };
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .status();
}
