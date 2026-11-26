//! Bakes a build identifier into `VTTEAMS_BUILD_VERSION` (read back with `env!`, see `ui::draw_header` and
//! `--version`): the nearest tag and commit, or just the short commit hash with `--always` when there is none,
//! `-dirty` if the working tree does not match it. Falls back to `"dev"` when `git` is not on `PATH` or the checkout
//! has no commit yet (`git describe` fails either way), so a source tarball or a fresh clone still builds and shows
//! something sensible instead of an empty string.
//!
//! Cargo reruns a build script only when something it was told to watch changes, and a commit changes no source
//! file, so the git state that `git describe` reads is watched explicitly: `HEAD`, the index, the branch refs and
//! `packed-refs`, wherever this checkout keeps them (a worktree or submodule has its own). The source folders are
//! watched too, for the `-dirty` marker.
use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|v| !v.is_empty())
}

fn main() {
    let version = git(&["describe", "--always", "--dirty"]).unwrap_or_else(|| "dev".to_string());
    println!("cargo:rustc-env=VTTEAMS_BUILD_VERSION={version}");

    println!("cargo:rerun-if-changed=build.rs");
    for source in ["src", "crates", "Cargo.toml", "Cargo.lock"] {
        println!("cargo:rerun-if-changed={source}");
    }
    // A path that does not exist makes Cargo rerun on every build, so only existing ones are named.
    for name in ["HEAD", "index", "refs", "packed-refs"] {
        if let Some(path) = git(&["rev-parse", "--git-path", name])
            && Path::new(&path).exists()
        {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}
