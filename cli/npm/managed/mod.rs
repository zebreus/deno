// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use cache::RegistryInfoDownloader;
use cache::TarballCache;
use deno_ast::ModuleSpecifier;
use deno_core::anyhow::Context;
use deno_core::error::AnyError;
use deno_core::serde_json;
use deno_npm::npm_rc::ResolvedNpmRc;
use deno_npm::registry::NpmPackageInfo;
use deno_npm::registry::NpmRegistryApi;
use deno_npm::resolution::NpmResolutionSnapshot;
use deno_npm::resolution::PackageReqNotFoundError;
use deno_npm::resolution::ValidSerializedNpmResolutionSnapshot;
use deno_npm::NpmPackageId;
use deno_npm::NpmResolutionPackage;
use deno_npm::NpmSystemInfo;
use deno_runtime::deno_fs::FileSystem;
use deno_runtime::deno_node::NodePermissions;
use deno_runtime::deno_node::NpmResolver;
use deno_semver::package::PackageNv;
use deno_semver::package::PackageReq;
use resolution::AddPkgReqsResult;

use crate::args::CliLockfile;
use crate::args::NpmProcessState;
use crate::args::NpmProcessStateKind;
use crate::args::PackageJsonDepsProvider;
use crate::cache::FastInsecureHasher;
use crate::http_util::HttpClientProvider;
use crate::util::fs::canonicalize_path_maybe_not_exists_with_fs;
use crate::util::progress_bar::ProgressBar;
use crate::util::sync::AtomicFlag;

use self::cache::NpmCache;
use self::registry::CliNpmRegistryApi;
use self::resolution::NpmResolution;
use self::resolvers::create_npm_fs_resolver;
use self::resolvers::NpmPackageFsResolver;

use super::CliNpmResolver;
use super::InnerCliNpmResolverRef;
use super::NpmCacheDir;

mod cache;
mod registry;
mod resolution;
mod resolvers;

pub enum CliNpmResolverManagedSnapshotOption {
  ResolveFromLockfile(Arc<CliLockfile>),
  Specified(Option<ValidSerializedNpmResolutionSnapshot>),
}

pub struct CliNpmResolverManagedCreateOptions {
  pub snapshot: CliNpmResolverManagedSnapshotOption,
  pub maybe_lockfile: Option<Arc<CliLockfile>>,
  pub fs: Arc<dyn deno_runtime::deno_fs::FileSystem>,
  pub http_client_provider: Arc<crate::http_util::HttpClientProvider>,
  pub npm_global_cache_dir: PathBuf,
  pub cache_setting: crate::args::CacheSetting,
  pub text_only_progress_bar: crate::util::progress_bar::ProgressBar,
  pub maybe_node_modules_path: Option<PathBuf>,
  pub npm_system_info: NpmSystemInfo,
  pub package_json_deps_provider: Arc<PackageJsonDepsProvider>,
  pub npmrc: Arc<ResolvedNpmRc>,
}

pub async fn create_managed_npm_resolver_for_lsp(
  options: CliNpmResolverManagedCreateOptions,
) -> Arc<dyn CliNpmResolver> {
  let npm_cache = create_cache(&options);
  let npm_api = create_api(&options, npm_cache.clone());
  // spawn due to the lsp's `Send` requirement
  deno_core::unsync::spawn(async move {
    let snapshot = match resolve_snapshot(&npm_api, options.snapshot).await {
      Ok(snapshot) => snapshot,
      Err(err) => {
        log::warn!("failed to resolve snapshot: {}", err);
        None
      }
    };
    create_inner(
      options.fs,
      options.http_client_provider,
      options.maybe_lockfile,
      npm_api,
      npm_cache,
      options.npmrc,
      options.package_json_deps_provider,
      options.text_only_progress_bar,
      options.maybe_node_modules_path,
      options.npm_system_info,
      snapshot,
    )
  })
  .await
  .unwrap()
}

pub async fn create_managed_npm_resolver(
  options: CliNpmResolverManagedCreateOptions,
) -> Result<Arc<dyn CliNpmResolver>, AnyError> {
  let npm_cache = create_cache(&options);
  let npm_api = create_api(&options, npm_cache.clone());
  let snapshot = resolve_snapshot(&npm_api, options.snapshot).await?;
  Ok(create_inner(
    options.fs,
    options.http_client_provider,
    options.maybe_lockfile,
    npm_api,
    npm_cache,
    options.npmrc,
    options.package_json_deps_provider,
    options.text_only_progress_bar,
    options.maybe_node_modules_path,
    options.npm_system_info,
    snapshot,
  ))
}

#[allow(clippy::too_many_arguments)]
fn create_inner(
  fs: Arc<dyn deno_runtime::deno_fs::FileSystem>,
  http_client_provider: Arc<HttpClientProvider>,
  maybe_lockfile: Option<Arc<CliLockfile>>,
  npm_api: Arc<CliNpmRegistryApi>,
  npm_cache: Arc<NpmCache>,
  npm_rc: Arc<ResolvedNpmRc>,
  package_json_deps_provider: Arc<PackageJsonDepsProvider>,
  text_only_progress_bar: crate::util::progress_bar::ProgressBar,
  node_modules_dir_path: Option<PathBuf>,
  npm_system_info: NpmSystemInfo,
  snapshot: Option<ValidSerializedNpmResolutionSnapshot>,
) -> Arc<dyn CliNpmResolver> {
  let resolution = Arc::new(NpmResolution::from_serialized(
    npm_api.clone(),
    snapshot,
    maybe_lockfile.clone(),
  ));
  let tarball_cache = Arc::new(TarballCache::new(
    npm_cache.clone(),
    fs.clone(),
    http_client_provider.clone(),
    npm_rc.clone(),
    text_only_progress_bar.clone(),
  ));
  let fs_resolver = create_npm_fs_resolver(
    fs.clone(),
    npm_cache.clone(),
    &text_only_progress_bar,
    resolution.clone(),
    tarball_cache.clone(),
    node_modules_dir_path,
    npm_system_info.clone(),
  );
  Arc::new(ManagedCliNpmResolver::new(
    fs,
    fs_resolver,
    maybe_lockfile,
    npm_api,
    npm_cache,
    package_json_deps_provider,
    resolution,
    tarball_cache,
    text_only_progress_bar,
    npm_system_info,
  ))
}

fn create_cache(options: &CliNpmResolverManagedCreateOptions) -> Arc<NpmCache> {
  Arc::new(NpmCache::new(
    NpmCacheDir::new(
      options.npm_global_cache_dir.clone(),
      options.npmrc.get_all_known_registries_urls(),
    ),
    options.cache_setting.clone(),
    options.npmrc.clone(),
  ))
}

fn create_api(
  options: &CliNpmResolverManagedCreateOptions,
  npm_cache: Arc<NpmCache>,
) -> Arc<CliNpmRegistryApi> {
  Arc::new(CliNpmRegistryApi::new(
    npm_cache.clone(),
    Arc::new(RegistryInfoDownloader::new(
      npm_cache,
      options.http_client_provider.clone(),
      options.npmrc.clone(),
      options.text_only_progress_bar.clone(),
    )),
  ))
}

async fn resolve_snapshot(
  api: &CliNpmRegistryApi,
  snapshot: CliNpmResolverManagedSnapshotOption,
) -> Result<Option<ValidSerializedNpmResolutionSnapshot>, AnyError> {
  match snapshot {
    CliNpmResolverManagedSnapshotOption::ResolveFromLockfile(lockfile) => {
      if !lockfile.overwrite() {
        let snapshot = snapshot_from_lockfile(lockfile.clone(), api)
          .await
          .with_context(|| {
            format!("failed reading lockfile '{}'", lockfile.filename.display())
          })?;
        Ok(Some(snapshot))
      } else {
        Ok(None)
      }
    }
    CliNpmResolverManagedSnapshotOption::Specified(snapshot) => Ok(snapshot),
  }
}

async fn snapshot_from_lockfile(
  lockfile: Arc<CliLockfile>,
  api: &dyn NpmRegistryApi,
) -> Result<ValidSerializedNpmResolutionSnapshot, AnyError> {
  let (incomplete_snapshot, skip_integrity_check) = {
    let lock = lockfile.lock();
    (
      deno_npm::resolution::incomplete_snapshot_from_lockfile(&lock)?,
      lock.overwrite,
    )
  };
  let snapshot = deno_npm::resolution::snapshot_from_lockfile(
    deno_npm::resolution::SnapshotFromLockfileParams {
      incomplete_snapshot,
      api,
      skip_integrity_check,
    },
  )
  .await?;
  Ok(snapshot)
}

/// An npm resolver where the resolution is managed by Deno rather than
/// the user bringing their own node_modules (BYONM) on the file system.
pub struct ManagedCliNpmResolver {
  fs: Arc<dyn FileSystem>,
  fs_resolver: Arc<dyn NpmPackageFsResolver>,
  maybe_lockfile: Option<Arc<CliLockfile>>,
  npm_api: Arc<CliNpmRegistryApi>,
  npm_cache: Arc<NpmCache>,
  package_json_deps_provider: Arc<PackageJsonDepsProvider>,
  resolution: Arc<NpmResolution>,
  tarball_cache: Arc<TarballCache>,
  text_only_progress_bar: ProgressBar,
  npm_system_info: NpmSystemInfo,
  top_level_install_flag: AtomicFlag,
}

impl std::fmt::Debug for ManagedCliNpmResolver {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ManagedNpmResolver")
      .field("<omitted>", &"<omitted>")
      .finish()
  }
}

impl ManagedCliNpmResolver {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    fs: Arc<dyn FileSystem>,
    fs_resolver: Arc<dyn NpmPackageFsResolver>,
    maybe_lockfile: Option<Arc<CliLockfile>>,
    npm_api: Arc<CliNpmRegistryApi>,
    npm_cache: Arc<NpmCache>,
    package_json_deps_provider: Arc<PackageJsonDepsProvider>,
    resolution: Arc<NpmResolution>,
    tarball_cache: Arc<TarballCache>,
    text_only_progress_bar: ProgressBar,
    npm_system_info: NpmSystemInfo,
  ) -> Self {
    Self {
      fs,
      fs_resolver,
      maybe_lockfile,
      npm_api,
      npm_cache,
      package_json_deps_provider,
      text_only_progress_bar,
      resolution,
      tarball_cache,
      npm_system_info,
      top_level_install_flag: Default::default(),
    }
  }

  pub fn resolve_pkg_folder_from_pkg_id(
    &self,
    pkg_id: &NpmPackageId,
  ) -> Result<PathBuf, AnyError> {
    let path = self.fs_resolver.package_folder(pkg_id)?;
    let path =
      canonicalize_path_maybe_not_exists_with_fs(&path, self.fs.as_ref())?;
    log::debug!(
      "Resolved package folder of {} to {}",
      pkg_id.as_serialized(),
      path.display()
    );
    Ok(path)
  }

  /// Resolves the package nv from the provided specifier.
  pub fn resolve_pkg_id_from_specifier(
    &self,
    specifier: &ModuleSpecifier,
  ) -> Result<Option<NpmPackageId>, AnyError> {
    let Some(cache_folder_id) = self
      .fs_resolver
      .resolve_package_cache_folder_id_from_specifier(specifier)?
    else {
      return Ok(None);
    };
    Ok(Some(
      self
        .resolution
        .resolve_pkg_id_from_pkg_cache_folder_id(&cache_folder_id)?,
    ))
  }

  pub fn resolve_pkg_reqs_from_pkg_id(
    &self,
    id: &NpmPackageId,
  ) -> Vec<PackageReq> {
    self.resolution.resolve_pkg_reqs_from_pkg_id(id)
  }

  /// Attempts to get the package size in bytes.
  pub fn package_size(
    &self,
    package_id: &NpmPackageId,
  ) -> Result<u64, AnyError> {
    let package_folder = self.fs_resolver.package_folder(package_id)?;
    Ok(crate::util::fs::dir_size(&package_folder)?)
  }

  pub fn all_system_packages(
    &self,
    system_info: &NpmSystemInfo,
  ) -> Vec<NpmResolutionPackage> {
    self.resolution.all_system_packages(system_info)
  }

  /// Checks if the provided package req's folder is cached.
  pub fn is_pkg_req_folder_cached(&self, req: &PackageReq) -> bool {
    self
      .resolve_pkg_id_from_pkg_req(req)
      .ok()
      .and_then(|id| self.fs_resolver.package_folder(&id).ok())
      .map(|folder| folder.exists())
      .unwrap_or(false)
  }

  /// Adds package requirements to the resolver and ensures everything is setup.
  pub async fn add_package_reqs(
    &self,
    packages: &[PackageReq],
  ) -> Result<(), AnyError> {
    let result = self.add_package_reqs_raw(packages).await;
    result.dependencies_result
  }

  pub async fn add_package_reqs_raw(
    &self,
    packages: &[PackageReq],
  ) -> AddPkgReqsResult {
    if packages.is_empty() {
      return AddPkgReqsResult {
        dependencies_result: Ok(()),
        results: vec![],
      };
    }

    let mut result = self.resolution.add_package_reqs(packages).await;
    if result.dependencies_result.is_ok() {
      result.dependencies_result =
        self.cache_packages().await.map_err(AnyError::from);
    }

    result
  }

  /// Sets package requirements to the resolver, removing old requirements and adding new ones.
  ///
  /// This will retrieve and resolve package information, but not cache any package files.
  pub async fn set_package_reqs(
    &self,
    packages: &[PackageReq],
  ) -> Result<(), AnyError> {
    self.resolution.set_package_reqs(packages).await
  }

  pub fn snapshot(&self) -> NpmResolutionSnapshot {
    self.resolution.snapshot()
  }

  pub fn serialized_valid_snapshot_for_system(
    &self,
    system_info: &NpmSystemInfo,
  ) -> ValidSerializedNpmResolutionSnapshot {
    self
      .resolution
      .serialized_valid_snapshot_for_system(system_info)
  }

  pub async fn inject_synthetic_types_node_package(
    &self,
  ) -> Result<(), AnyError> {
    // add and ensure this isn't added to the lockfile
    self
      .add_package_reqs(&[PackageReq::from_str("@types/node").unwrap()])
      .await?;

    Ok(())
  }

  pub async fn cache_packages(&self) -> Result<(), AnyError> {
    self.fs_resolver.cache_packages().await
  }

  pub fn resolve_pkg_folder_from_deno_module(
    &self,
    nv: &PackageNv,
  ) -> Result<PathBuf, AnyError> {
    let pkg_id = self.resolution.resolve_pkg_id_from_deno_module(nv)?;
    self.resolve_pkg_folder_from_pkg_id(&pkg_id)
  }

  fn resolve_pkg_id_from_pkg_req(
    &self,
    req: &PackageReq,
  ) -> Result<NpmPackageId, PackageReqNotFoundError> {
    self.resolution.resolve_pkg_id_from_pkg_req(req)
  }

  pub async fn ensure_top_level_package_json_install(
    &self,
  ) -> Result<(), AnyError> {
    let Some(reqs) = self.package_json_deps_provider.reqs() else {
      return Ok(());
    };
    if !self.top_level_install_flag.raise() {
      return Ok(()); // already did this
    }
    // check if something needs resolving before bothering to load all
    // the package information (which is slow)
    if reqs
      .iter()
      .all(|req| self.resolution.resolve_pkg_id_from_pkg_req(req).is_ok())
    {
      log::debug!(
        "All package.json deps resolvable. Skipping top level install."
      );
      return Ok(()); // everything is already resolvable
    }

    let reqs = reqs.into_iter().cloned().collect::<Vec<_>>();
    self.add_package_reqs(&reqs).await
  }

  pub async fn cache_package_info(
    &self,
    package_name: &str,
  ) -> Result<Arc<NpmPackageInfo>, AnyError> {
    // this will internally cache the package information
    self
      .npm_api
      .package_info(package_name)
      .await
      .map_err(|err| err.into())
  }

  pub fn global_cache_root_folder(&self) -> PathBuf {
    self.npm_cache.root_folder()
  }
}

impl NpmResolver for ManagedCliNpmResolver {
  /// Gets the state of npm for the process.
  fn get_npm_process_state(&self) -> String {
    serde_json::to_string(&NpmProcessState {
      kind: NpmProcessStateKind::Snapshot(
        self
          .resolution
          .serialized_valid_snapshot()
          .into_serialized(),
      ),
      local_node_modules_path: self
        .fs_resolver
        .node_modules_path()
        .map(|p| p.to_string_lossy().to_string()),
    })
    .unwrap()
  }

  fn resolve_package_folder_from_package(
    &self,
    name: &str,
    referrer: &ModuleSpecifier,
  ) -> Result<PathBuf, AnyError> {
    let path = self
      .fs_resolver
      .resolve_package_folder_from_package(name, referrer)?;
    let path =
      canonicalize_path_maybe_not_exists_with_fs(&path, self.fs.as_ref())?;
    log::debug!("Resolved {} from {} to {}", name, referrer, path.display());
    Ok(path)
  }

  fn in_npm_package(&self, specifier: &ModuleSpecifier) -> bool {
    let root_dir_url = self.fs_resolver.root_dir_url();
    debug_assert!(root_dir_url.as_str().ends_with('/'));
    specifier.as_ref().starts_with(root_dir_url.as_str())
  }

  fn ensure_read_permission(
    &self,
    permissions: &mut dyn NodePermissions,
    path: &Path,
  ) -> Result<(), AnyError> {
    self.fs_resolver.ensure_read_permission(permissions, path)
  }
}

impl CliNpmResolver for ManagedCliNpmResolver {
  fn into_npm_resolver(self: Arc<Self>) -> Arc<dyn NpmResolver> {
    self
  }

  fn clone_snapshotted(&self) -> Arc<dyn CliNpmResolver> {
    // create a new snapshotted npm resolution and resolver
    let npm_resolution = Arc::new(NpmResolution::new(
      self.npm_api.clone(),
      self.resolution.snapshot(),
      self.maybe_lockfile.clone(),
    ));

    Arc::new(ManagedCliNpmResolver::new(
      self.fs.clone(),
      create_npm_fs_resolver(
        self.fs.clone(),
        self.npm_cache.clone(),
        &self.text_only_progress_bar,
        npm_resolution.clone(),
        self.tarball_cache.clone(),
        self.root_node_modules_path().map(ToOwned::to_owned),
        self.npm_system_info.clone(),
      ),
      self.maybe_lockfile.clone(),
      self.npm_api.clone(),
      self.npm_cache.clone(),
      self.package_json_deps_provider.clone(),
      npm_resolution,
      self.tarball_cache.clone(),
      self.text_only_progress_bar.clone(),
      self.npm_system_info.clone(),
    ))
  }

  fn as_inner(&self) -> InnerCliNpmResolverRef {
    InnerCliNpmResolverRef::Managed(self)
  }

  fn root_node_modules_path(&self) -> Option<&PathBuf> {
    self.fs_resolver.node_modules_path()
  }

  fn resolve_pkg_folder_from_deno_module_req(
    &self,
    req: &PackageReq,
    _referrer: &ModuleSpecifier,
  ) -> Result<PathBuf, AnyError> {
    let pkg_id = self.resolve_pkg_id_from_pkg_req(req)?;
    self.resolve_pkg_folder_from_pkg_id(&pkg_id)
  }

  fn check_state_hash(&self) -> Option<u64> {
    // We could go further and check all the individual
    // npm packages, but that's probably overkill.
    let mut package_reqs = self
      .resolution
      .package_reqs()
      .into_iter()
      .collect::<Vec<_>>();
    package_reqs.sort_by(|a, b| a.0.cmp(&b.0)); // determinism
    let mut hasher = FastInsecureHasher::new_without_deno_version();
    // ensure the cache gets busted when turning nodeModulesDir on or off
    // as this could cause changes in resolution
    hasher.write_hashable(self.fs_resolver.node_modules_path().is_some());
    for (pkg_req, pkg_nv) in package_reqs {
      hasher.write_hashable(&pkg_req);
      hasher.write_hashable(&pkg_nv);
    }
    Some(hasher.finish())
  }
}
