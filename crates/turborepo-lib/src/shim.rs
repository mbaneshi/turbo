use std::{
    env,
    env::{current_dir, current_exe},
    fs::{self, File},
    path::{Path, PathBuf},
    process,
    process::Stdio,
    str::FromStr,
};

use anyhow::{anyhow, Result};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use tiny_gradient::{GradientStr, RGB};
use turbo_updater::check_for_updates;

use crate::{cli, get_version, PackageManager, Payload};

static TURBO_JSON: &str = "turbo.json";
// all arguments that result in a stdout that much be directly parsable and
// should not be paired with additional output (from the update notifier for
// example)
static TURBO_PURE_OUTPUT_ARGS: [&str; 6] = [
    "--json",
    "--dry",
    "--dry-run",
    "--dry=json",
    "--graph",
    "--dry-run=json",
];

static SUPPORTS_SKIP_INFER_SEMVER: &str = ">=1.7.0-canary.0";

#[derive(Debug)]
struct ShimArgs {
    cwd: PathBuf,
    skip_infer: bool,
    remaining_turbo_args: Vec<String>,
    forwarded_args: Vec<String>,
}

impl ShimArgs {
    pub fn parse() -> Result<Self> {
        let mut found_cwd_flag = false;
        let mut cwd: Option<PathBuf> = None;
        let mut skip_infer = false;
        let mut remaining_turbo_args = Vec::new();
        let mut forwarded_args = Vec::new();
        let mut is_forwarded_args = false;
        let args = env::args().skip(1);
        for arg in args {
            // We've seen a `--` and therefore we do no parsing
            if is_forwarded_args {
                forwarded_args.push(arg);
            } else if arg == "--skip-infer" {
                skip_infer = true;
            } else if arg == "--" {
                // If we've hit `--` we've reached the args forwarded to tasks.
                is_forwarded_args = true;
            } else if found_cwd_flag {
                // We've seen a `--cwd` and therefore set the cwd to this arg.
                cwd = Some(arg.into());
                found_cwd_flag = false;
            } else if arg == "--cwd" {
                if cwd.is_some() {
                    return Err(anyhow!("cannot have multiple `--cwd` flags in command"));
                }
                // If we see a `--cwd` we expect the next arg to be a path.
                found_cwd_flag = true
            } else {
                remaining_turbo_args.push(arg);
            }
        }

        if found_cwd_flag {
            Err(anyhow!("No value assigned to `--cwd` argument"))
        } else {
            let cwd = if let Some(cwd) = cwd {
                cwd
            } else {
                current_dir()?
            };

            Ok(ShimArgs {
                cwd,
                skip_infer,
                remaining_turbo_args,
                forwarded_args,
            })
        }
    }

    // returns true if any flags result in pure json output to stdout
    pub fn has_json_flags(&self) -> bool {
        self.remaining_turbo_args
            .iter()
            .any(|arg| TURBO_PURE_OUTPUT_ARGS.contains(&arg.as_str()))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum RepoMode {
    SinglePackage,
    MultiPackage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PackageJson {
    version: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoState {
    pub root: PathBuf,
    pub mode: RepoMode,
}

impl RepoState {
    /// Infers `RepoState` from current directory.
    ///
    /// # Arguments
    ///
    /// * `current_dir`: Current working directory
    ///
    /// returns: Result<RepoState, Error>
    pub fn infer(current_dir: &Path) -> Result<Self> {
        // First we look for a `turbo.json`. This iterator returns the first ancestor
        // that contains a `turbo.json` file.
        let root_path = current_dir
            .ancestors()
            .find(|p| fs::metadata(p.join(TURBO_JSON)).is_ok());

        // If that directory exists, then we figure out if there are workspaces defined
        // in it NOTE: This may change with multiple `turbo.json` files
        if let Some(root_path) = root_path {
            let pnpm = PackageManager::Pnpm;
            let npm = PackageManager::Npm;
            let is_workspace = pnpm.get_workspace_globs(root_path).is_ok()
                || npm.get_workspace_globs(root_path).is_ok();

            let mode = if is_workspace {
                RepoMode::MultiPackage
            } else {
                RepoMode::SinglePackage
            };

            return Ok(Self {
                root: root_path.to_path_buf(),
                mode,
            });
        }

        // What we look for next is a directory that contains a `package.json`.
        let potential_roots = current_dir
            .ancestors()
            .filter(|path| fs::metadata(path.join("package.json")).is_ok());

        let mut first_package_json_dir = None;
        // We loop through these directories and see if there are workspaces defined in
        // them, either in the `package.json` or `pnm-workspaces.yml`
        for dir in potential_roots {
            if first_package_json_dir.is_none() {
                first_package_json_dir = Some(dir)
            }

            let pnpm = PackageManager::Pnpm;
            let npm = PackageManager::Npm;
            let is_workspace =
                pnpm.get_workspace_globs(dir).is_ok() || npm.get_workspace_globs(dir).is_ok();

            if is_workspace {
                return Ok(Self {
                    root: dir.to_path_buf(),
                    mode: RepoMode::MultiPackage,
                });
            }
        }

        // Finally, if we don't detect any workspaces, go to the first `package.json`
        // and use that in single package mode.
        let root = first_package_json_dir
            .ok_or_else(|| {
                anyhow!(
                    "Unable to find `{}` or `package.json` in current path",
                    TURBO_JSON
                )
            })?
            .to_path_buf();

        Ok(Self {
            root,
            mode: RepoMode::SinglePackage,
        })
    }

    /// Attempts to run correct turbo by finding nearest package.json,
    /// then finding local turbo installation. If the current binary is the
    /// local turbo installation, then we run current turbo. Otherwise we
    /// kick over to the local turbo installation.
    ///
    /// # Arguments
    ///
    /// * `turbo_state`: state for current execution
    ///
    /// returns: Result<i32, Error>
    fn run_correct_turbo(self, shim_args: ShimArgs) -> Result<Payload> {
        let local_turbo_path = self.root.join("node_modules").join(".bin").join({
            #[cfg(windows)]
            {
                "turbo.cmd"
            }
            #[cfg(not(windows))]
            {
                "turbo"
            }
        });

        if should_run_current_turbo(&local_turbo_path)? {
            cli::run(Some(self))
        } else {
            let canonical_local_turbo = local_turbo_path.canonicalize()?;
            // Otherwise we spawn the local turbo process.
            Ok(Payload::Rust(
                self.spawn_local_turbo(&canonical_local_turbo, shim_args),
            ))
        }
    }

    fn local_turbo_supports_skip_infer(&self) -> Result<bool> {
        let local_turbo_package_path = self
            .root
            .join("node_modules")
            .join("turbo")
            .join("package.json");
        let package_json: PackageJson =
            serde_json::from_reader(File::open(local_turbo_package_path)?)?;
        let version = Version::from_str(&package_json.version)?;
        let skip_infer_versions = VersionReq::parse(SUPPORTS_SKIP_INFER_SEMVER).unwrap();
        Ok(skip_infer_versions.matches(&version))
    }

    fn spawn_local_turbo(&self, local_turbo_path: &Path, mut shim_args: ShimArgs) -> Result<i32> {
        println!(
            "Running local turbo binary in {}\n",
            local_turbo_path.display()
        );

        let cwd = self.root.canonicalize()?;
        let mut raw_args: Vec<_> = if self.local_turbo_supports_skip_infer()? {
            vec!["--skip-infer".to_string()]
        } else {
            Vec::new()
        };

        let has_single_package_flag = shim_args
            .remaining_turbo_args
            .contains(&"--single-package".to_string());

        raw_args.append(&mut shim_args.remaining_turbo_args);
        if self.mode == RepoMode::SinglePackage && !has_single_package_flag {
            raw_args.push("--single-package".to_string());
        }

        raw_args.push("--".to_string());
        raw_args.append(&mut shim_args.forwarded_args);

        // We spawn a process that executes the local turbo
        // that we've found in node_modules/.bin/turbo.
        let mut command = process::Command::new(local_turbo_path)
            .args(&raw_args)
            .current_dir(cwd)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("Failed to execute turbo.");

        Ok(command.wait()?.code().unwrap_or(2))
    }
}

/// If the local turbo path doesn't exist or if we are local turbo, then we go
/// ahead and run the Go code linked in the current binary.
fn should_run_current_turbo(local_turbo_path: &Path) -> Result<bool> {
    // Note we must check if local_turbo_path exists before we
    // canonicalize the path, otherwise we'll get an error.
    if !local_turbo_path.exists() {
        return Ok(true);
    }

    Ok(local_turbo_path.canonicalize()? == current_exe()?.canonicalize()?)
}

/// Checks for `TURBO_BINARY_PATH` variable. If it is set,
/// we do not try to find local turbo, we simply run the command as
/// the current binary. This is due to legacy behavior of `TURBO_BINARY_PATH`
/// that lets users dynamically set the path of the turbo binary. Because
/// that conflicts with finding a local turbo installation and
/// executing that binary, these two features are fundamentally incompatible.
fn is_turbo_binary_path_set() -> bool {
    env::var("TURBO_BINARY_PATH").is_ok()
}

pub fn run() -> Result<Payload> {
    let args = ShimArgs::parse()?;
    // If skip_infer is passed, we're probably running local turbo with
    // global turbo having handled the inference. We can run without any
    // concerns.

    if !args.has_json_flags() {
        // custom footer for update message
        let footer = format!(
            "Follow {username} for updates: {url}",
            username = "@turborepo".gradient([RGB::new(0, 153, 247), RGB::new(241, 23, 18)]),
            url = "https://twitter.com/turborepo"
        );

        // check for updates
        let _ = check_for_updates(
            "turbo",
            "https://github.com/vercel/turbo",
            Some(&footer),
            get_version(),
            // use defaults for timeout and refresh interval (800ms and 1 day respectively)
            None,
            None,
        );
    }

    if args.skip_infer {
        return cli::run(None);
    }

    // If the TURBO_BINARY_PATH is set, we do inference but we do not use
    // it to execute local turbo. We simply use it to set the `--single-package`
    // and `--cwd` flags.
    if is_turbo_binary_path_set() {
        let repo_state = RepoState::infer(&args.cwd)?;
        return cli::run(Some(repo_state));
    }

    match RepoState::infer(&args.cwd) {
        Ok(repo_state) => repo_state.run_correct_turbo(args),
        Err(err) => {
            // If we cannot infer, we still run global turbo. This allows for global
            // commands like login/logout/link/unlink to still work
            eprintln!("Repository inference failed: {}", err);
            eprintln!("Running command as global turbo");
            cli::run(None)
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_skip_infer_version_constraint() {
        let req = VersionReq::parse(SUPPORTS_SKIP_INFER_SEMVER).unwrap();
        let canary = Version::parse("1.7.0-canary.0").unwrap();
        let release = Version::parse("1.7.0").unwrap();
        let old = Version::parse("1.6.3").unwrap();
        let new = Version::parse("1.8.0").unwrap();
        assert!(req.matches(&release));
        assert!(req.matches(&canary));
        assert!(req.matches(&new));
        assert!(!req.matches(&old));
    }
}