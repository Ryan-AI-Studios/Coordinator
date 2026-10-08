//! Git commands target the repository selected by their cwd, even inside a Git hook.

use std::process::Command;

// Repository-local variables reported by `git rev-parse --local-env-vars`.
// In particular, pre-push exports GIT_DIR: changing cwd alone cannot override it.
const REPOSITORY_ENV: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_OBJECT_DIRECTORY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
];

pub(crate) fn clear_repository_env(command: &mut Command) {
    for key in REPOSITORY_ENV {
        command.env_remove(key);
    }
}

pub(crate) fn command() -> Command {
    let mut command = Command::new("git");
    clear_repository_env(&mut command);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_environment_cannot_redirect_init_or_index_to_foreign_repository() {
        let foreign = tempfile::tempdir().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let init = command()
            .arg("init")
            .current_dir(foreign.path())
            .output()
            .unwrap();
        assert!(init.status.success());
        let config = std::fs::read(foreign.path().join(".git/config")).unwrap();
        std::fs::write(fixture.path().join("README.md"), "fixture\n").unwrap();

        for args in [&["init"][..], &["add", "README.md"][..]] {
            let mut child = Command::new("git");
            child
                .args(args)
                .current_dir(fixture.path())
                .env("GIT_DIR", foreign.path().join(".git"))
                .env("GIT_WORK_TREE", foreign.path())
                .env("GIT_COMMON_DIR", foreign.path().join(".git"))
                .env("GIT_INDEX_FILE", foreign.path().join(".git/index"))
                .env("GIT_PREFIX", "foreign/");
            clear_repository_env(&mut child);
            let output = child.output().unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert!(fixture.path().join(".git/index").is_file());
        assert!(!foreign.path().join(".git/index").exists());
        assert_eq!(
            std::fs::read(foreign.path().join(".git/config")).unwrap(),
            config
        );
    }
}
