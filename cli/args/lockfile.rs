// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

use std::collections::BTreeSet;
use std::path::PathBuf;

use deno_config::deno_json::ConfigFile;
use deno_config::workspace::Workspace;
use deno_core::anyhow::Context;
use deno_core::error::AnyError;
use deno_core::parking_lot::Mutex;
use deno_core::parking_lot::MutexGuard;
use deno_lockfile::WorkspaceMemberConfig;
use deno_package_json::PackageJsonDepValue;
use deno_runtime::deno_node::PackageJson;

use crate::cache;
use crate::util::fs::atomic_write_file_with_retries;
use crate::Flags;

use crate::args::DenoSubcommand;
use crate::args::InstallFlags;
use crate::args::InstallKind;

use deno_lockfile::Lockfile;

#[derive(Debug)]
pub struct CliLockfile {
  lockfile: Mutex<Lockfile>,
  pub filename: PathBuf,
  pub frozen: bool,
}

pub struct Guard<'a, T> {
  guard: MutexGuard<'a, T>,
}

impl<'a, T> std::ops::Deref for Guard<'a, T> {
  type Target = T;

  fn deref(&self) -> &Self::Target {
    &self.guard
  }
}

impl<'a, T> std::ops::DerefMut for Guard<'a, T> {
  fn deref_mut(&mut self) -> &mut Self::Target {
    &mut self.guard
  }
}

impl CliLockfile {
  pub fn new(lockfile: Lockfile, frozen: bool) -> Self {
    let filename = lockfile.filename().clone();
    Self {
      lockfile: Mutex::new(lockfile),
      filename,
      frozen,
    }
  }

  /// Get the inner deno_lockfile::Lockfile.
  pub fn lock(&self) -> Guard<Lockfile> {
    Guard {
      guard: self.lockfile.lock(),
    }
  }

  pub fn set_workspace_config(
    &self,
    options: deno_lockfile::SetWorkspaceConfigOptions,
  ) {
    self.lockfile.lock().set_workspace_config(options);
  }

  pub fn overwrite(&self) -> bool {
    self.lockfile.lock().overwrite()
  }

  pub fn write_if_changed(&self) -> Result<(), AnyError> {
    self.error_if_changed()?;
    let mut lockfile = self.lockfile.lock();
    let Some(bytes) = lockfile.resolve_write_bytes() else {
      return Ok(()); // nothing to do
    };
    // do an atomic write to reduce the chance of multiple deno
    // processes corrupting the file
    atomic_write_file_with_retries(
      &lockfile.filename(),
      bytes,
      cache::CACHE_PERM,
    )
    .context("Failed writing lockfile.")?;
    Ok(())
  }

  pub fn discover(
    flags: &Flags,
    workspace: &Workspace,
  ) -> Result<Option<CliLockfile>, AnyError> {
    fn pkg_json_deps(maybe_pkg_json: Option<&PackageJson>) -> BTreeSet<String> {
      let Some(pkg_json) = maybe_pkg_json else {
        return Default::default();
      };
      pkg_json
        .resolve_local_package_json_deps()
        .values()
        .filter_map(|dep| dep.as_ref().ok())
        .filter_map(|dep| match dep {
          PackageJsonDepValue::Req(req) => Some(req),
          PackageJsonDepValue::Workspace(_) => None,
        })
        .map(|r| format!("npm:{}", r))
        .collect()
    }

    fn deno_json_deps(
      maybe_deno_json: Option<&ConfigFile>,
    ) -> BTreeSet<String> {
      maybe_deno_json
        .map(|c| {
          crate::args::deno_json::deno_json_deps(c)
            .into_iter()
            .map(|req| req.to_string())
            .collect()
        })
        .unwrap_or_default()
    }

    if flags.no_lock
      || matches!(
        flags.subcommand,
        DenoSubcommand::Install(InstallFlags {
          kind: InstallKind::Global(..),
          ..
        }) | DenoSubcommand::Uninstall(_)
      )
    {
      return Ok(None);
    }

    let filename = match flags.lock {
      Some(ref lock) => PathBuf::from(lock),
      None => match workspace.resolve_lockfile_path()? {
        Some(path) => path,
        None => return Ok(None),
      },
    };

    let lockfile = if flags.lock_write {
      log::warn!(
        "{} \"--lock-write\" flag is deprecated and will be removed in Deno 2.",
        crate::colors::yellow("Warning")
      );
      CliLockfile::new(Lockfile::new(filename, true), flags.frozen_lockfile)
    } else {
      Self::read_from_path(filename, flags.frozen_lockfile)?
    };

    // initialize the lockfile with the workspace's configuration
    let root_url = workspace.root_dir();
    let root_folder = workspace.root_folder_configs();
    let config = deno_lockfile::WorkspaceConfig {
      root: WorkspaceMemberConfig {
        package_json_deps: pkg_json_deps(root_folder.pkg_json.as_deref()),
        dependencies: deno_json_deps(root_folder.deno_json.as_deref()),
      },
      members: workspace
        .config_folders()
        .iter()
        .filter(|(folder_url, _)| *folder_url != root_url)
        .filter_map(|(folder_url, folder)| {
          Some((
            {
              // should never be None here, but just ignore members that
              // do fail for this
              let mut relative_path = root_url.make_relative(folder_url)?;
              if relative_path.ends_with('/') {
                // make it slightly cleaner by removing the trailing slash
                relative_path.pop();
              }
              relative_path
            },
            {
              let config = WorkspaceMemberConfig {
                package_json_deps: pkg_json_deps(folder.pkg_json.as_deref()),
                dependencies: deno_json_deps(folder.deno_json.as_deref()),
              };
              if config.package_json_deps.is_empty()
                && config.dependencies.is_empty()
              {
                // exclude empty workspace members
                return None;
              }
              config
            },
          ))
        })
        .collect(),
    };
    lockfile.set_workspace_config(deno_lockfile::SetWorkspaceConfigOptions {
      no_npm: flags.no_npm,
      no_config: flags.config_flag == super::ConfigFlag::Disabled,
      config,
    });

    Ok(Some(lockfile))
  }
  pub fn read_from_path(
    filename: PathBuf,
    frozen: bool,
  ) -> Result<CliLockfile, AnyError> {
    match std::fs::read_to_string(&filename) {
      Ok(text) => Ok(CliLockfile::new(
        Lockfile::with_lockfile_content(filename, &text, false)?,
        frozen,
      )),
      Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
        Ok(CliLockfile::new(Lockfile::new(filename, false), frozen))
      }
      Err(err) => Err(err).with_context(|| {
        format!("Failed reading lockfile '{}'", filename.display())
      }),
    }
  }
  pub fn error_if_changed(&self) -> Result<(), AnyError> {
    if !self.frozen {
      return Ok(());
    }
    let lockfile = self.lockfile.lock();
    if lockfile.has_content_changed() {
      let suggested = if *super::DENO_FUTURE {
        "`deno cache --frozen=false`, `deno install --frozen=false`,"
      } else {
        "`deno cache --frozen=false`"
      };

      let contents =
        std::fs::read_to_string(lockfile.filename()).unwrap_or_default();
      let new_contents = lockfile.to_json();
      let diff = crate::util::diff::diff(&contents, &new_contents);
      // has an extra newline at the end
      let diff = diff.trim_end();
      Err(deno_core::anyhow::anyhow!(
        "The lockfile is out of date. Run {suggested} or rerun with `--frozen=false` to update it.\nchanges:\n{diff}"
      ))
    } else {
      Ok(())
    }
  }
}
