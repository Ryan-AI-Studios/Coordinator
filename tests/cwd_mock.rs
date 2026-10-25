fn main() {
    let out = std::env::var("COORDINATOR_CWD_OUT").expect("COORDINATOR_CWD_OUT");
    let cwd = std::env::current_dir().expect("cwd");
    std::fs::write(&out, cwd.display().to_string()).expect("write cwd");
    if let Ok(pid_path) = std::env::var("COORDINATOR_CWD_PID") {
        let _ = std::fs::write(pid_path, std::process::id().to_string());
    }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(30));
    }
}
