use super::workspaces;
use std::fs::{self, Metadata};
use std::io::{self, ErrorKind};
use std::path::Path;
use tokio_util::sync::CancellationToken;

const INSTALL_INPUTS: &[&str] =
    &["package-lock.json", "npm-shrinkwrap.json", "pnpm-lock.yaml", "yarn.lock", "bun.lock", "bun.lockb", ".npmrc"];

const SUPPORTED_VITEST_MAJOR: &str = "3";

pub(super) fn prepare(
    source_repo: &Path,
    checkout: &Path,
    env_root: &Path,
    cancel: &CancellationToken,
) -> Result<(), String> {
    let frozen_manifest = dependency_file(&checkout.join("package.json"))?;
    let operator_manifest = dependency_file(&source_repo.join("package.json"))?;
    for (dir, manifest, label) in [
        (checkout, frozen_manifest.as_deref(), "the checkout"),
        (source_repo, operator_manifest.as_deref(), "the repository"),
    ] {
        if let Some(kind) = workspaces(dir, manifest) {
            return Err(format!(
                "{label} is a JavaScript workspace ({kind}); LYRA supports one package at a time and \
                 workspaces are not supported yet"
            ));
        }
    }
    if operator_manifest != frozen_manifest {
        return Err(baseline_mismatch("package.json"));
    }
    for name in INSTALL_INPUTS {
        if dependency_file(&source_repo.join(name))? != dependency_file(&checkout.join(name))? {
            return Err(baseline_mismatch(name));
        }
    }

    check_checkout(checkout)?;
    snapshot(&source_repo.join("node_modules"), env_root, cancel)
}

fn baseline_mismatch(name: &str) -> String {
    format!(
        "the installed dependency checkout differs from the frozen baseline in {name}; \
         check out the same baseline in the operator repository, reinstall its dependencies, and retry"
    )
}

pub(super) fn snapshot(installed: &Path, env_root: &Path, cancel: &CancellationToken) -> Result<(), String> {
    if !plain_dir(installed) {
        return Err(missing_vitest(installed));
    }
    let source = dunce::canonicalize(installed).map_err(|error| at(installed, error))?;
    check_vitest(&source)?;
    let parent = dunce::canonicalize(env_root).map_err(|error| at(env_root, error))?;
    let destination = parent.join("node_modules");
    if destination.starts_with(&source) || source.starts_with(&destination) {
        return Err("the dependency snapshot must be outside the operator's node_modules".into());
    }
    cancelled(cancel)?;
    remove_tree(&destination).map_err(|error| at(&destination, error))?;
    fs::create_dir(&destination).map_err(|error| at(&destination, error))?;
    let result = copy_entries(&source, &source, &destination, cancel).and_then(|()| check(&parent));
    if let Err(reason) = result {
        if let Err(error) = remove_tree(&destination) {
            return Err(format!("{reason}; cleaning up {}: {error}", destination.display()));
        }
        return Err(reason);
    }
    Ok(())
}

fn missing_vitest(installed: &Path) -> String {
    format!(
        "{} has no installed Vitest in a local node_modules directory; install the baseline's dependencies \
         (for example with npm ci or pnpm install --frozen-lockfile) and retry",
        installed.display()
    )
}

fn check_vitest(source: &Path) -> Result<(), String> {
    let path = source.join("vitest/package.json");
    let resolved = match dunce::canonicalize(&path) {
        Ok(resolved) if resolved.starts_with(source) => resolved,
        Ok(_) => return Err(outside_install(&path)),
        Err(error) if error.kind() == ErrorKind::NotFound => return Err(missing_vitest(source)),
        Err(error) => return Err(at(&path, error)),
    };
    let version = dependency_file(&resolved)?
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|manifest| manifest.get("version").and_then(serde_json::Value::as_str).map(str::to_owned))
        .ok_or_else(|| missing_vitest(source))?;
    if version.split('.').next() != Some(SUPPORTED_VITEST_MAJOR) {
        return Err(format!(
            "the repository has Vitest {version} installed; the js-vitest profile supports Vitest \
             {SUPPORTED_VITEST_MAJOR}.x, whose module graph it reads to establish which code the test ran"
        ));
    }
    Ok(())
}

fn outside_install(path: &Path) -> String {
    format!("{} points outside the installed node_modules; install standalone dependencies and retry", path.display())
}

pub(super) fn check_checkout(checkout: &Path) -> Result<(), String> {
    let path = checkout.join("node_modules");
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(at(&path, error)),
        Ok(_) => Err(format!("{} would shadow the private Vitest dependencies", path.display())),
    }
}

pub(super) fn check(env_root: &Path) -> Result<(), String> {
    let directory = env_root.join("node_modules");
    let vitest = directory.join("vitest");
    if plain_dir(&directory)
        && vitest.is_dir()
        && dunce::canonicalize(&directory)
            .and_then(|root| dunce::canonicalize(&vitest).map(|package| package.starts_with(&root)))
            .unwrap_or(false)
    {
        Ok(())
    } else {
        Err(format!("{} is missing the private Vitest dependencies", directory.display()))
    }
}

fn dependency_file(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() && !is_link(&meta) => fs::read(path).map(Some).map_err(|error| at(path, error)),
        Ok(_) => Err(format!("{} must be a regular file to compare dependency baselines", path.display())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(at(path, error)),
    }
}

fn plain_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir() && !is_link(&meta))
}

fn is_link(meta: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

fn remove_tree(path: &Path) -> io::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    #[cfg(windows)]
    if is_link(&meta) {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
        if meta.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0 {
            return fs::remove_dir(path);
        }
    }
    if meta.is_dir() && !is_link(&meta) {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn copy_entries(dir: &Path, source: &Path, destination: &Path, cancel: &CancellationToken) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|error| at(dir, error))? {
        cancelled(cancel)?;
        let entry = entry.map_err(|error| at(dir, error))?;
        if dir == source && matches!(entry.file_name().to_str(), Some(".vite" | ".vite-temp" | ".vitest")) {
            continue;
        }
        let path = entry.path();
        let target = destination.join(path.strip_prefix(source).expect("traversed from source"));
        let meta = fs::symlink_metadata(&path).map_err(|error| at(&path, error))?;
        if is_link(&meta) {
            let resolved = dunce::canonicalize(&path).map_err(|error| at(&path, error))?;
            let relative = resolved.strip_prefix(source).map_err(|_| outside_install(&path))?;
            let directory = fs::metadata(&resolved).map_err(|error| at(&resolved, error))?.is_dir();
            internal_link(&destination.join(relative), &target, directory).map_err(|error| at(&target, error))?;
        } else if meta.is_dir() {
            fs::create_dir(&target).map_err(|error| at(&target, error))?;
            copy_entries(&path, source, destination, cancel)?;
        } else if meta.is_file() {
            fs::copy(&path, &target).map_err(|error| at(&path, error))?;
        } else {
            return Err(format!("{} is not a regular dependency file, directory or internal link", path.display()));
        }
    }
    cancelled(cancel)
}

fn cancelled(cancel: &CancellationToken) -> Result<(), String> {
    if cancel.is_cancelled() {
        Err("preparing the Vitest dependency snapshot was cancelled".into())
    } else {
        Ok(())
    }
}

fn at(path: &Path, error: io::Error) -> String {
    format!("{}: {error}", path.display())
}

#[cfg(unix)]
pub(super) fn internal_link(target: &Path, link: &Path, _directory: bool) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
pub(super) fn internal_link(target: &Path, link: &Path, directory: bool) -> io::Result<()> {
    if directory {
        junction(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

/// Recreate a package-manager directory link without cmd or symbolic-link privileges.
/// The NTFS mount-point buffer stores the absolute substitute and display paths in UTF-16.
#[cfg(windows)]
fn junction(target: &Path, link: &Path) -> io::Result<()> {
    use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle};
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::GENERIC_WRITE;
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT};
    use windows_sys::Win32::System::{
        Ioctl::FSCTL_SET_REPARSE_POINT, SystemServices::IO_REPARSE_TAG_MOUNT_POINT, IO::DeviceIoControl,
    };

    let target: Vec<u16> = target
        .as_os_str()
        .encode_wide()
        .map(|unit| if unit == u16::from(b'/') { u16::from(b'\\') } else { unit })
        .collect();
    let prefix: Vec<u16> = r"\\?\".encode_utf16().collect();
    let target = target.strip_prefix(&prefix[..]).unwrap_or(&target);
    let substitute: Vec<u16> = r"\??\".encode_utf16().chain(target.iter().copied()).collect();
    let (substitute_len, print_len) = (substitute.len() * 2, target.len() * 2);
    let data_len = 8 + substitute_len + print_len + 4;
    if data_len + 8 > 16 * 1024 {
        return Err(io::Error::new(ErrorKind::InvalidInput, "junction target path is too long"));
    }
    let mut buffer = Vec::with_capacity(data_len + 8);
    buffer.extend(IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    for value in [data_len, 0, 0, substitute_len, substitute_len + 2, print_len] {
        buffer.extend((value as u16).to_le_bytes());
    }
    for value in substitute.into_iter().chain([0]).chain(target.iter().copied()).chain([0]) {
        buffer.extend(value.to_le_bytes());
    }

    fs::create_dir(link)?;
    let file = fs::OpenOptions::new()
        .access_mode(GENERIC_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(link)?;
    let mut returned = 0;
    // SAFETY: the directory handle and the initialized input buffer live through this synchronous call.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_REPARSE_POINT,
            buffer.as_ptr().cast(),
            buffer.len() as u32,
            null_mut(),
            0,
            &mut returned,
            null_mut(),
        )
    };
    if ok != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        repo: std::path::PathBuf,
        checkout: std::path::PathBuf,
        run: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::Builder::new().prefix("lyra Vitest & deps ").tempdir().unwrap();
            let repo = dir.path().join("operator");
            let run = dir.path().join("run");
            let checkout = run.join("checkout");
            fs::create_dir_all(repo.join("node_modules/vitest")).unwrap();
            fs::write(repo.join("node_modules/vitest/package.json"), r#"{"version":"3.2.4"}"#).unwrap();
            fs::create_dir_all(&checkout).unwrap();
            for base in [&repo, &checkout] {
                fs::write(base.join("package.json"), "{}").unwrap();
            }
            Self { _dir: dir, repo, checkout, run }
        }

        fn prepare(&self) -> Result<(), String> {
            prepare(&self.repo, &self.checkout, &self.run, &CancellationToken::new())
        }
    }

    #[test]
    fn internal_package_links_use_private_targets_and_external_links_fail_closed() {
        let fixture = Fixture::new();
        let installed = fixture.repo.join("node_modules");
        let target = installed.join(".pnpm/helper/node_modules/helper");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("index.js"), "original").unwrap();
        internal_link(&target, &installed.join("helper"), true).unwrap();
        // A leftover junction from an older run must be unlinked without deleting its target.
        internal_link(&installed, &fixture.run.join("node_modules"), true).unwrap();
        fixture.prepare().unwrap();

        let private = fixture.run.join("node_modules/helper/index.js");
        fs::write(&private, "changed").unwrap();
        assert_eq!(fs::read_to_string(target.join("index.js")).unwrap(), "original");
        assert_eq!(
            dunce::canonicalize(&private).unwrap(),
            dunce::canonicalize(fixture.run.join("node_modules/.pnpm/helper/node_modules/helper/index.js")).unwrap()
        );

        internal_link(&fixture.repo, &installed.join("foreign"), true).unwrap();
        let error = fixture.prepare().unwrap_err();
        assert!(error.contains("points outside the installed node_modules"), "{error}");
        assert!(!fixture.run.join("node_modules").exists(), "failed copy is cleaned up");
        assert_eq!(fs::read_to_string(target.join("index.js")).unwrap(), "original");
    }

    #[test]
    fn invalid_baseline_inputs_do_not_count_as_missing_files() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.checkout.join("yarn.lock")).unwrap();
        let error = fixture.prepare().unwrap_err();
        assert!(error.contains("yarn.lock") && error.contains("regular file"), "{error}");
        assert!(!fixture.run.join("node_modules").exists());
    }

    #[test]
    fn a_snapshot_cannot_be_nested_inside_the_install_it_copies() {
        let fixture = Fixture::new();
        let installed = fixture.repo.join("node_modules");
        let nested = installed.join("runs/id");
        fs::create_dir_all(&nested).unwrap();
        let error = snapshot(&installed, &nested, &CancellationToken::new()).unwrap_err();
        assert!(error.contains("must be outside"), "{error}");
        assert!(!nested.join("node_modules").exists());
    }

    #[test]
    fn workspace_markers_are_checked_in_the_actual_checkout() {
        let fixture = Fixture::new();
        for marker in ["pnpm-workspace.yaml", "vitest.workspace.ts", "vitest.projects.js", "vitest.workspace.json"] {
            let path = fixture.checkout.join(marker);
            fs::write(&path, "").unwrap();
            let error = fixture.prepare().unwrap_err();
            assert!(error.contains("workspaces are not supported"), "{error}");
            fs::remove_file(path).unwrap();
        }
        fs::write(fixture.checkout.join("package.json"), r#"{"workspaces":["packages/*"]}"#).unwrap();
        assert!(fixture.prepare().unwrap_err().contains("package.json workspaces"));
    }

    #[test]
    fn a_checkout_cannot_shadow_the_private_install() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.checkout.join("node_modules")).unwrap();
        assert!(fixture.prepare().unwrap_err().contains("would shadow"));
    }

    #[test]
    fn cancellation_leaves_no_partial_dependency_tree() {
        let fixture = Fixture::new();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = prepare(&fixture.repo, &fixture.checkout, &fixture.run, &cancel).unwrap_err();
        assert!(error.contains("cancelled"), "{error}");
        assert!(!fixture.run.join("node_modules").exists());
    }
}
