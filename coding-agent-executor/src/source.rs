use crate::{ExecutorError, ExecutorErrorCode, PrepareRequest, SourceInspection, ValidatedConfig};

#[cfg(target_os = "linux")]
pub(crate) fn inspect(
    config: &ValidatedConfig,
    request: &PrepareRequest,
) -> Result<SourceInspection, ExecutorError> {
    use crate::git_context::GitContext;
    use crate::workspace::{prepared_record, string_args};

    let (prepared, workspace) = prepared_record(
        config,
        &request.run_id,
        &request.attempt_id,
        &request.repository,
        &request.base_branch,
        &request.base_commit,
    )?;
    let root = crate::artifact::open_directory(&workspace)?;
    let git_name = std::ffi::CString::new(".git").unwrap();
    let source_git =
        crate::artifact::openat_directory(std::os::fd::AsRawFd::as_raw_fd(&root), &git_name)?;
    let git = GitContext::new(&config.state_root)?;
    let args = |parts: &[&str]| {
        let mut args = string_args(&[
            &format!("--git-dir={}", workspace.join(".git").display()),
            &format!("--work-tree={}", workspace.display()),
            "-c",
            "core.bare=false",
        ]);
        args.extend(string_args(parts));
        args
    };
    let branch = git.checked(
        &workspace,
        &args(&["symbolic-ref", "--short", "HEAD"]),
        None,
    )?;
    let branch = parse_git_line(branch)?;
    if branch != prepared.result.branch {
        return Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace));
    }
    let head_commit = parse_git_line(git.checked(
        &workspace,
        &args(&["rev-parse", "--verify", "HEAD^{commit}"]),
        None,
    )?)?;
    crate::validate_full_commit(&head_commit)?;
    let object_format = parse_git_line(git.checked(
        &workspace,
        &args(&["rev-parse", "--show-object-format"]),
        None,
    )?)?;
    if !matches!(object_format.as_str(), "sha1" | "sha256") {
        return Err(unsafe_workspace());
    }
    let scratch = InspectionGitDirectory::new(config, &git, &workspace, &object_format)?;
    let inspection_args = |parts: &[&str]| {
        let mut args = string_args(&[
            &format!("--git-dir={}", scratch.0.display()),
            &format!("--work-tree={}", workspace.display()),
            "-c",
            "core.bare=false",
        ]);
        args.extend(string_args(parts));
        args
    };
    if !git
        .run(
            &workspace,
            &inspection_args(&[
                "merge-base",
                "--is-ancestor",
                &request.base_commit,
                &head_commit,
            ]),
            None,
        )?
        .success
    {
        return Err(ExecutorError::new(
            ExecutorErrorCode::CommitNotBasedOnApprovedBase,
        ));
    }
    // Copy data files, not guest configuration or filter programs, into the private repository.
    if !copy_git_data_file(&source_git, "index", &scratch.0.join("index"))? {
        git.checked(
            &workspace,
            &inspection_args(&["read-tree", &head_commit]),
            None,
        )?;
    }
    let shared = String::from_utf8(git.checked(
        &workspace,
        &args(&["rev-parse", "--shared-index-path"]),
        None,
    )?)
    .map_err(|_| unsafe_workspace())?;
    let shared = shared.trim_end_matches('\n');
    if !shared.is_empty() {
        let shared_path = std::path::Path::new(shared);
        let name = shared_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(unsafe_workspace)?;
        let hash = name
            .strip_prefix("sharedindex.")
            .ok_or_else(unsafe_workspace)?;
        crate::validate_full_commit(hash)?;
        if shared_path != workspace.join(".git").join(name)
            && shared_path != std::path::Path::new(".git").join(name)
        {
            return Err(unsafe_workspace());
        }
        if !copy_git_data_file(&source_git, name, &scratch.0.join(name))? {
            return Err(unsafe_workspace());
        }
    }
    let info_name = std::ffi::CString::new("info").unwrap();
    let info = crate::artifact::openat_directory(
        std::os::fd::AsRawFd::as_raw_fd(&source_git),
        &info_name,
    )?;
    copy_git_data_file(&info, "exclude", &scratch.0.join("info/exclude"))?;
    // Retain staged paths and object IDs, but discard guest-controlled skip
    // flags and cached stat data before comparing actual worktree contents.
    let staged_entries = git.checked_limited(
        &workspace,
        &inspection_args(&["ls-files", "--stage", "-z"]),
        None,
        8 * 1024 * 1024,
    )?;
    git.checked(
        &workspace,
        &inspection_args(&["read-tree", "--empty"]),
        None,
    )?;
    if !git
        .run_with_input(
            &workspace,
            &inspection_args(&["update-index", "-z", "--index-info"]),
            None,
            &staged_entries,
        )?
        .success
    {
        return Err(unsafe_workspace());
    }
    let limit = config.max_import_bytes.min(8 * 1024 * 1024) as usize;
    let tracked_diff = git.checked_limited(
        &workspace,
        &inspection_args(&[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            &request.base_commit,
            "--",
            ".",
            ":(exclude).coding-agent",
            ":(exclude).coding-agent/**",
        ]),
        None,
        limit,
    )?;
    let uncommitted_diff = git.checked_limited(
        &workspace,
        &inspection_args(&[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            &head_commit,
            "--",
            ".",
            ":(exclude).coding-agent",
            ":(exclude).coding-agent/**",
        ]),
        None,
        limit
            .checked_sub(tracked_diff.len())
            .ok_or_else(too_large)?,
    )?;
    let names = git.checked_limited(
        &workspace,
        &inspection_args(&[
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            ".",
            ":(exclude).coding-agent",
            ":(exclude).coding-agent/**",
        ]),
        None,
        limit,
    )?;
    let mut remaining = limit
        .checked_sub(tracked_diff.len())
        .and_then(|remaining| remaining.checked_sub(uncommitted_diff.len()))
        .ok_or_else(too_large)?;
    let mut new_files = Vec::new();
    for name in names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        if new_files.len() >= 65_536 {
            return Err(too_large());
        }
        remaining = remaining.checked_sub(name.len()).ok_or_else(too_large)?;
        let path = std::str::from_utf8(name).map_err(|_| unsafe_workspace())?;
        let file = read_new_file(&root, path, remaining)?;
        remaining = remaining
            .checked_sub(file.content.len())
            .ok_or_else(too_large)?;
        new_files.push(file);
    }
    // A changed HEAD or branch invalidates this read rather than mixing attempts.
    if parse_git_line(git.checked(
        &workspace,
        &args(&["rev-parse", "--verify", "HEAD^{commit}"]),
        None,
    )?)? != head_commit
        || parse_git_line(git.checked(
            &workspace,
            &args(&["symbolic-ref", "--short", "HEAD"]),
            None,
        )?)? != branch
    {
        return Err(unsafe_workspace());
    }
    let inspection = SourceInspection {
        run_id: request.run_id.clone(),
        attempt_id: request.attempt_id.clone(),
        base_commit: request.base_commit.clone(),
        head_commit,
        branch,
        tracked_diff,
        uncommitted_diff: Some(uncommitted_diff),
        new_files,
    };
    if serde_json::to_vec(&inspection)
        .map_err(|_| unsafe_workspace())?
        .len()
        > crate::MAX_SOURCE_RESPONSE_BYTES - 512
    {
        return Err(too_large());
    }
    Ok(inspection)
}

#[cfg(target_os = "linux")]
struct InspectionGitDirectory(std::path::PathBuf);

#[cfg(target_os = "linux")]
pub(crate) fn import_commit(
    config: &ValidatedConfig,
    request: &PrepareRequest,
    commit: &str,
    publisher: &std::path::Path,
) -> Result<(), ExecutorError> {
    use crate::{git_context::GitContext, workspace::string_args};
    use std::os::fd::AsRawFd;
    let (_, workspace) = crate::workspace::prepared_record(
        config,
        &request.run_id,
        &request.attempt_id,
        &request.repository,
        &request.base_branch,
        &request.base_commit,
    )?;
    let root = crate::artifact::open_directory(&workspace)?;
    let git_dir = crate::artifact::openat_directory(
        root.as_raw_fd(),
        &std::ffi::CString::new(".git").unwrap(),
    )?;
    let objects = crate::artifact::openat_directory(
        git_dir.as_raw_fd(),
        &std::ffi::CString::new("objects").unwrap(),
    )?;
    let git = GitContext::new(&config.state_root)?;
    let format = if commit.len() == 40 { "sha1" } else { "sha256" };
    let scratch = InspectionGitDirectory::new(config, &git, &workspace, format)?;
    std::fs::remove_file(scratch.0.join("objects/info/alternates"))
        .map_err(|_| unsafe_workspace())?;
    // Copy regular object data through directory descriptors. Guest Git configuration,
    // hooks and alternate object stores must never execute or supply publication data.
    let mut remaining = config.max_import_bytes;
    copy_object_directory(&objects, &scratch.0.join("objects"), &mut remaining, 0)?;
    std::fs::write(scratch.0.join("refs/heads/export"), format!("{commit}\n"))
        .map_err(|_| unsafe_workspace())?;
    git.checked(
        publisher,
        &string_args(&[
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            "--",
            scratch.0.to_str().ok_or_else(unsafe_workspace)?,
            commit,
        ]),
        None,
    )?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn copy_object_directory(
    source: &std::fs::File,
    target: &std::path::Path,
    remaining: &mut u64,
    depth: usize,
) -> Result<(), ExecutorError> {
    use std::{io::Read, os::fd::AsRawFd};
    if depth > 4 {
        return Err(unsafe_workspace());
    }
    let entries = std::fs::read_dir(format!("/proc/self/fd/{}", source.as_raw_fd()))
        .map_err(|_| unsafe_workspace())?;
    for entry in entries {
        let name = entry.map_err(|_| unsafe_workspace())?.file_name();
        let text = name.to_str().ok_or_else(unsafe_workspace)?;
        if text == "alternates" {
            return Err(unsafe_workspace());
        }
        let c_name = std::ffi::CString::new(text).map_err(|_| unsafe_workspace())?;
        let stat = crate::artifact::stat_at(source.as_raw_fd(), &c_name)?;
        let destination = target.join(&name);
        match stat.st_mode & libc::S_IFMT {
            libc::S_IFDIR => {
                let directory = crate::artifact::openat_directory(source.as_raw_fd(), &c_name)?;
                if !destination.exists() {
                    std::fs::create_dir(&destination).map_err(|_| unsafe_workspace())?;
                }
                copy_object_directory(&directory, &destination, remaining, depth + 1)?;
            }
            libc::S_IFREG => {
                let file = crate::artifact::openat_file(source.as_raw_fd(), &c_name)?;
                let size = file.metadata().map_err(|_| unsafe_workspace())?.len();
                if size > *remaining {
                    return Err(too_large());
                }
                let mut contents = Vec::new();
                file.take(size + 1)
                    .read_to_end(&mut contents)
                    .map_err(|_| unsafe_workspace())?;
                if contents.len() as u64 != size {
                    return Err(unsafe_workspace());
                }
                *remaining -= size;
                std::fs::write(&destination, contents).map_err(|_| unsafe_workspace())?;
            }
            _ => return Err(unsafe_workspace()),
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn import_commit(
    _: &ValidatedConfig,
    _: &PrepareRequest,
    _: &str,
    _: &std::path::Path,
) -> Result<(), ExecutorError> {
    Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace))
}

#[cfg(target_os = "linux")]
impl InspectionGitDirectory {
    fn new(
        config: &ValidatedConfig,
        git: &crate::git_context::GitContext,
        workspace: &std::path::Path,
        format: &str,
    ) -> Result<Self, ExecutorError> {
        use std::{
            fs,
            os::unix::fs::DirBuilderExt,
            sync::atomic::{AtomicU64, Ordering},
        };
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let directory = config.state_root.join(format!(
            "source-inspection-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .map_err(|_| unsafe_workspace())?;
        let scratch = Self(directory);
        git.checked(
            &config.state_root,
            &crate::workspace::string_args(&[
                "init",
                "--bare",
                &format!("--object-format={format}"),
                scratch.0.to_str().ok_or_else(unsafe_workspace)?,
            ]),
            None,
        )?;
        let objects = workspace.join(".git/objects");
        crate::artifact::open_directory(&objects)?;
        let objects = objects
            .to_str()
            .filter(|path| !path.chars().any(char::is_control))
            .ok_or_else(unsafe_workspace)?;
        fs::write(
            scratch.0.join("objects/info/alternates"),
            format!("{objects}\n"),
        )
        .map_err(|_| unsafe_workspace())?;
        Ok(scratch)
    }
}

#[cfg(target_os = "linux")]
impl Drop for InspectionGitDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn inspect(
    _: &ValidatedConfig,
    _: &PrepareRequest,
) -> Result<SourceInspection, ExecutorError> {
    Err(ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace))
}

#[cfg(target_os = "linux")]
fn parse_git_line(bytes: Vec<u8>) -> Result<String, ExecutorError> {
    let text = String::from_utf8(bytes).map_err(|_| unsafe_workspace())?;
    let text = text.strip_suffix('\n').ok_or_else(unsafe_workspace)?;
    if text.is_empty() || text.chars().any(char::is_control) {
        return Err(unsafe_workspace());
    }
    Ok(text.to_owned())
}

#[cfg(target_os = "linux")]
fn read_new_file(
    root: &std::fs::File,
    path: &str,
    limit: usize,
) -> Result<crate::SourceFile, ExecutorError> {
    use crate::artifact::{openat_directory, openat_file, stat_at};
    use std::{ffi::CString, io::Read, os::fd::AsRawFd};
    let mut parent = root.try_clone().map_err(|_| unsafe_workspace())?;
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        if part.is_empty() || matches!(part, "." | ".." | ".git") {
            return Err(unsafe_workspace());
        }
        let name = CString::new(part).map_err(|_| unsafe_workspace())?;
        if parts.peek().is_some() {
            parent = openat_directory(parent.as_raw_fd(), &name)?;
            continue;
        }
        let stat = stat_at(parent.as_raw_fd(), &name)?;
        let is_symlink = stat.st_mode & libc::S_IFMT == libc::S_IFLNK;
        let content = if is_symlink {
            let mut target = vec![0u8; limit.min(4096) + 1];
            let size = unsafe {
                libc::readlinkat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    target.as_mut_ptr().cast(),
                    target.len(),
                )
            };
            if size < 0 {
                return Err(unsafe_workspace());
            }
            if size as usize >= target.len() {
                return Err(too_large());
            }
            target.truncate(size as usize);
            target
        } else {
            if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
                return Err(unsafe_workspace());
            }
            let file = openat_file(parent.as_raw_fd(), &name)?;
            let metadata = file.metadata().map_err(|_| unsafe_workspace())?;
            if !metadata.is_file() {
                return Err(unsafe_workspace());
            }
            if metadata.len() > limit as u64 {
                return Err(too_large());
            }
            let mut bytes = Vec::new();
            file.take(limit as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| unsafe_workspace())?;
            if bytes.len() > limit {
                return Err(too_large());
            }
            bytes
        };
        return Ok(crate::SourceFile {
            path: path.to_owned(),
            is_symlink,
            content,
        });
    }
    Err(unsafe_workspace())
}

#[cfg(target_os = "linux")]
fn unsafe_workspace() -> ExecutorError {
    ExecutorError::new(ExecutorErrorCode::UnsafeWorkspace)
}

#[cfg(target_os = "linux")]
fn too_large() -> ExecutorError {
    ExecutorError::new(ExecutorErrorCode::ResponseTooLarge)
}

#[cfg(target_os = "linux")]
fn copy_git_data_file(
    parent: &std::fs::File,
    name: &str,
    target: &std::path::Path,
) -> Result<bool, ExecutorError> {
    use std::{ffi::CString, io::Read, os::fd::AsRawFd};
    let name = CString::new(name).map_err(|_| unsafe_workspace())?;
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
            return Ok(false);
        }
        return Err(unsafe_workspace());
    }
    let file = crate::artifact::openat_file(parent.as_raw_fd(), &name)?;
    let metadata = file.metadata().map_err(|_| unsafe_workspace())?;
    if !metadata.is_file() {
        return Err(unsafe_workspace());
    }
    let limit = 8 * 1024 * 1024;
    if metadata.len() > limit {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| unsafe_workspace())?;
    if bytes.len() as u64 > limit {
        return Err(too_large());
    }
    std::fs::write(target, bytes).map_err(|_| unsafe_workspace())?;
    Ok(true)
}
