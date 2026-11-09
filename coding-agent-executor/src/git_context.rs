use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::ResolvedCredentialSource;
use crate::model::{ExecutorError, ExecutorErrorCode};

const ASKPASS_MARKER: &str = "CODING_AGENT_EXECUTOR_ASKPASS";
const ASKPASS_USERNAME: &str = "CODING_AGENT_EXECUTOR_ASKPASS_USERNAME";
const ASKPASS_TOKEN_FILE: &str = "CODING_AGENT_EXECUTOR_ASKPASS_TOKEN_FILE";
const MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024;
const GIT_OPERATION_TIMEOUT: Duration = Duration::from_secs(120);
pub(crate) const GIT_CHECK_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const GIT_CHECK_OUTPUT_BYTES: usize = 64 * 1024;

pub(crate) struct GitContext {
    git_executable: PathBuf,
    home: PathBuf,
    askpass_executable: PathBuf,
    ssh_command: Option<String>,
}

pub(crate) struct GitOutput {
    pub(crate) success: bool,
    pub(crate) stdout: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GitCheckFailure {
    Failed,
    Timeout,
    ResponseTooLarge,
}

impl GitCheckFailure {
    fn into_executor_error(self) -> ExecutorError {
        ExecutorError::new(match self {
            Self::ResponseTooLarge => ExecutorErrorCode::ResponseTooLarge,
            Self::Failed | Self::Timeout => ExecutorErrorCode::GitOperationFailed,
        })
    }
}

impl GitContext {
    pub(crate) fn new(state_root: &Path) -> Result<Self, ExecutorError> {
        let git_executable = ["/usr/bin/git", "/bin/git", "/usr/local/bin/git"]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())
            .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::GitOperationFailed))?;
        let askpass_executable = std::env::current_exe()
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::GitOperationFailed))?;
        Ok(Self {
            git_executable,
            home: state_root.join("git-home"),
            askpass_executable,
            ssh_command: None,
        })
    }

    pub(crate) fn for_repository(
        state_root: &Path,
        repository: &crate::RepositoryConfig,
    ) -> Result<Self, ExecutorError> {
        let mut git = Self::new(state_root)?;
        if let Some(directory) = &repository.ssh_directory {
            let directory = crate::validate_ssh_directory(directory)?;
            let ssh = ["/usr/bin/ssh", "/bin/ssh"]
                .into_iter()
                .map(PathBuf::from)
                .find(|path| path.is_file())
                .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::GitOperationFailed))?;
            git.ssh_command = Some(ssh_command(&ssh, &directory)?);
        }
        Ok(git)
    }

    pub(crate) fn run(
        &self,
        cwd: &Path,
        args: &[OsString],
        credential: Option<&ResolvedCredentialSource>,
    ) -> Result<GitOutput, ExecutorError> {
        self.run_limited(cwd, args, credential, MAX_CAPTURE_BYTES)
    }

    pub(crate) fn run_limited(
        &self,
        cwd: &Path,
        args: &[OsString],
        credential: Option<&ResolvedCredentialSource>,
        max_stdout_bytes: usize,
    ) -> Result<GitOutput, ExecutorError> {
        self.run_core(cwd, args, credential, None, max_stdout_bytes)
            .map_err(GitCheckFailure::into_executor_error)
    }

    pub(crate) fn run_with_input(
        &self,
        cwd: &Path,
        args: &[OsString],
        credential: Option<&ResolvedCredentialSource>,
        input: &[u8],
    ) -> Result<GitOutput, ExecutorError> {
        self.run_core(
            cwd,
            args,
            credential,
            Some(Box::new(std::io::Cursor::new(input.to_vec()))),
            MAX_CAPTURE_BYTES,
        )
        .map_err(GitCheckFailure::into_executor_error)
    }

    pub(crate) fn run_with_reader<R>(
        &self,
        cwd: &Path,
        args: &[OsString],
        credential: Option<&ResolvedCredentialSource>,
        input: R,
        max_stdout_bytes: usize,
    ) -> Result<GitOutput, ExecutorError>
    where
        R: Read + Send + 'static,
    {
        self.run_core(
            cwd,
            args,
            credential,
            Some(Box::new(input)),
            max_stdout_bytes,
        )
        .map_err(GitCheckFailure::into_executor_error)
    }

    pub(crate) fn check_remote(
        &self,
        cwd: &Path,
        args: &[OsString],
        credential: Option<&ResolvedCredentialSource>,
    ) -> Result<GitOutput, GitCheckFailure> {
        self.run_core_with_timeout(
            cwd,
            args,
            credential,
            None,
            GIT_CHECK_OUTPUT_BYTES,
            GIT_CHECK_TIMEOUT,
        )
    }

    fn run_core(
        &self,
        cwd: &Path,
        args: &[OsString],
        credential: Option<&ResolvedCredentialSource>,
        input: Option<Box<dyn Read + Send>>,
        max_stdout_bytes: usize,
    ) -> Result<GitOutput, GitCheckFailure> {
        self.run_core_with_timeout(
            cwd,
            args,
            credential,
            input,
            max_stdout_bytes,
            GIT_OPERATION_TIMEOUT,
        )
    }

    fn run_core_with_timeout(
        &self,
        cwd: &Path,
        args: &[OsString],
        credential: Option<&ResolvedCredentialSource>,
        input: Option<Box<dyn Read + Send>>,
        max_stdout_bytes: usize,
        timeout: Duration,
    ) -> Result<GitOutput, GitCheckFailure> {
        let mut command = self.command(cwd, args, credential);
        command
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        set_process_group(&mut command);
        let mut child = command.spawn().map_err(|_| GitCheckFailure::Failed)?;
        let stdout = child.stdout.take().ok_or(GitCheckFailure::Failed)?;
        let input_result_receiver = if let Some(input) = input {
            let mut stdin = child.stdin.take().ok_or(GitCheckFailure::Failed)?;
            let (sender, receiver) = mpsc::channel();
            thread::spawn(move || {
                let result = std::io::copy(&mut std::io::BufReader::new(input), &mut stdin);
                let _ = sender.send(result.map(|_| ()));
            });
            Some(receiver)
        } else {
            None
        };
        let (sender, output_receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = stdout
                .take(max_stdout_bytes.saturating_add(1) as u64)
                .read_to_end(&mut bytes)
                .map(|_| bytes);
            let _ = sender.send(result);
        });

        let start = Instant::now();
        let mut captured = None;
        let mut input_complete = input_result_receiver.is_none();
        let status = loop {
            if !input_complete
                && let Some(receiver) = input_result_receiver.as_ref()
                && let Ok(result) = receiver.try_recv()
            {
                if result.is_err() {
                    kill_process_tree(&mut child);
                    return Err(GitCheckFailure::Failed);
                }
                input_complete = true;
            }
            if captured.is_none()
                && let Ok(result) = output_receiver.try_recv()
            {
                let bytes = result.map_err(|_| GitCheckFailure::Failed)?;
                match validate_output_size(bytes, max_stdout_bytes) {
                    Ok(bytes) => captured = Some(bytes),
                    Err(error) => {
                        kill_process_tree(&mut child);
                        return Err(error);
                    }
                }
            }
            if let Some(status) = child.try_wait().map_err(|_| GitCheckFailure::Failed)? {
                break status;
            }
            if start.elapsed() >= timeout {
                kill_process_tree(&mut child);
                return Err(GitCheckFailure::Timeout);
            }
            thread::sleep(Duration::from_millis(10));
        };
        if captured.is_none() {
            let bytes = output_receiver
                .recv_timeout(Duration::from_secs(1))
                .map_err(|_| GitCheckFailure::Failed)?
                .map_err(|_| GitCheckFailure::Failed)?;
            captured = Some(validate_output_size(bytes, max_stdout_bytes)?);
        }
        if !input_complete && let Some(receiver) = input_result_receiver {
            receiver
                .recv_timeout(Duration::from_secs(1))
                .map_err(|_| GitCheckFailure::Failed)?
                .map_err(|_| GitCheckFailure::Failed)?;
        }
        Ok(GitOutput {
            success: status.success(),
            stdout: captured.unwrap_or_default(),
        })
    }

    pub(crate) fn run_silent(
        &self,
        cwd: &Path,
        args: &[OsString],
        credential: Option<&ResolvedCredentialSource>,
    ) -> Result<bool, ExecutorError> {
        let mut command = self.command(cwd, args, credential);
        command.stdout(Stdio::null()).stderr(Stdio::null());
        set_process_group(&mut command);
        let mut child = command
            .spawn()
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::GitOperationFailed))?;
        wait_with_timeout(&mut child).map(|status| status.success())
    }

    pub(crate) fn checked(
        &self,
        cwd: &Path,
        args: &[OsString],
        credential: Option<&ResolvedCredentialSource>,
    ) -> Result<Vec<u8>, ExecutorError> {
        let output = self.run(cwd, args, credential)?;
        if !output.success {
            return Err(ExecutorError::new(ExecutorErrorCode::GitOperationFailed));
        }
        Ok(output.stdout)
    }

    pub(crate) fn checked_limited(
        &self,
        cwd: &Path,
        args: &[OsString],
        credential: Option<&ResolvedCredentialSource>,
        max_stdout_bytes: usize,
    ) -> Result<Vec<u8>, ExecutorError> {
        let output = self.run_limited(cwd, args, credential, max_stdout_bytes)?;
        if !output.success {
            return Err(ExecutorError::new(ExecutorErrorCode::GitOperationFailed));
        }
        Ok(output.stdout)
    }

    fn command(
        &self,
        cwd: &Path,
        args: &[OsString],
        credential: Option<&ResolvedCredentialSource>,
    ) -> Command {
        let mut command = Command::new(&self.git_executable);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/local/bin")
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.home)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PAGER", "cat")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .current_dir(cwd);
        for config in [
            "core.hooksPath=/dev/null",
            "core.fsmonitor=false",
            "core.attributesFile=/dev/null",
            "core.autocrlf=false",
            "core.symlinks=false",
            "core.sshCommand=/dev/null",
            "credential.helper=",
            "protocol.allow=never",
            "protocol.https.allow=always",
            "protocol.http.allow=always",
            "protocol.file.allow=always",
            "protocol.ext.allow=never",
            "filter.lfs.smudge=",
            "filter.lfs.required=false",
            "gc.auto=0",
        ] {
            command.arg("-c").arg(config);
        }
        if let Some(ssh_command) = &self.ssh_command {
            command
                .arg("-c")
                .arg(format!("core.sshCommand={ssh_command}"))
                .arg("-c")
                .arg("protocol.ssh.allow=always");
        }
        command.args(args);
        if let Some(credential) = credential {
            command
                .env("GIT_ASKPASS", &self.askpass_executable)
                .env("GIT_ASKPASS_REQUIRE", "force")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env(ASKPASS_MARKER, "1")
                .env(ASKPASS_USERNAME, &credential.username)
                .env(ASKPASS_TOKEN_FILE, &credential.token_file);
        }
        command
    }
}

fn validate_output_size(bytes: Vec<u8>, max_bytes: usize) -> Result<Vec<u8>, GitCheckFailure> {
    if bytes.len() > max_bytes {
        Err(GitCheckFailure::ResponseTooLarge)
    } else {
        Ok(bytes)
    }
}

fn ssh_command(program: &Path, directory: &Path) -> Result<String, ExecutorError> {
    let config = directory.join("config");
    let mut args = vec![
        program.to_path_buf().into_os_string(),
        "-F".into(),
        if config.exists() {
            config.into_os_string()
        } else {
            "/dev/null".into()
        },
    ];
    for option in [
        "BatchMode=yes",
        "StrictHostKeyChecking=yes",
        "GlobalKnownHostsFile=/dev/null",
        "IdentityAgent=none",
        "IdentitiesOnly=yes",
        "IdentityFile=none",
    ] {
        args.extend(["-o".into(), option.into()]);
    }
    args.extend([
        "-o".into(),
        format!(
            "UserKnownHostsFile=\"{}\"",
            directory.join("known_hosts").display()
        )
        .into(),
    ]);
    for name in [
        "id_ed25519",
        "id_ecdsa",
        "id_rsa",
        "id_dsa",
        "id_ed25519_sk",
        "id_ecdsa_sk",
    ] {
        let key = directory.join(name);
        if key.is_file() {
            args.extend(["-i".into(), key.into_os_string()]);
        }
    }
    args.into_iter()
        .map(|arg| {
            let arg = arg
                .to_str()
                .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::InvalidConfiguration))?;
            if arg.chars().any(char::is_control) {
                return Err(ExecutorError::new(ExecutorErrorCode::InvalidConfiguration));
            }
            Ok(format!("'{}'", arg.replace('\'', "'\\''")))
        })
        .collect::<Result<Vec<_>, ExecutorError>>()
        .map(|args| args.join(" "))
}

fn wait_with_timeout(child: &mut Child) -> Result<std::process::ExitStatus, ExecutorError> {
    let start = Instant::now();
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::GitOperationFailed))?
        {
            return Ok(status);
        }
        if start.elapsed() >= GIT_OPERATION_TIMEOUT {
            kill_process_tree(child);
            return Err(ExecutorError::new(ExecutorErrorCode::GitOperationFailed));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn set_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
fn set_process_group(_command: &mut Command) {}

fn kill_process_tree(child: &mut Child) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(child.id()) {
        // `process_group(0)` above makes this child's PID its unique process
        // group ID, so this cannot target an inherited host process group.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub(crate) fn strings(parts: &[&str]) -> Vec<OsString> {
    parts.iter().map(OsString::from).collect()
}

/// Git invokes `GIT_ASKPASS` directly. This mode reads only the credential
/// source selected by trusted host configuration and writes one response to
/// Git's private pipe. It never emits diagnostics or receives request data.
pub fn run_askpass_mode() -> Option<i32> {
    if std::env::var_os(ASKPASS_MARKER).as_deref() != Some(OsStr::new("1")) {
        return None;
    }
    Some(match askpass_response() {
        Ok(()) => 0,
        Err(()) => 1,
    })
}

fn askpass_response() -> Result<(), ()> {
    let prompt = std::env::args_os().nth(1).ok_or(())?;
    let prompt = prompt.to_string_lossy().to_ascii_lowercase();
    let response = if prompt.contains("username") {
        std::env::var(ASKPASS_USERNAME).map_err(|_| ())?
    } else {
        let path = std::env::var_os(ASKPASS_TOKEN_FILE).ok_or(())?;
        let file = open_nofollow(Path::new(&path)).map_err(|_| ())?;
        if !file.metadata().map_err(|_| ())?.is_file() {
            return Err(());
        }
        let mut bytes = Vec::new();
        file.take(64 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ())?;
        if bytes.len() > 64 * 1024 {
            return Err(());
        }
        while matches!(bytes.last(), Some(b'\n' | b'\r')) {
            bytes.pop();
        }
        if bytes.is_empty() || bytes.iter().any(|byte| matches!(byte, 0 | b'\n' | b'\r')) {
            return Err(());
        }
        String::from_utf8(bytes).map_err(|_| ())?
    };
    if response.is_empty() || response.chars().any(char::is_control) {
        return Err(());
    }
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(response.as_bytes()).map_err(|_| ())?;
    stdout.write_all(b"\n").map_err(|_| ())?;
    stdout.flush().map_err(|_| ())
}

fn open_nofollow(path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        OpenOptions::new().read(true).open(path)
    }
}

#[cfg(all(test, unix))]
mod ssh_tests {
    use super::*;

    #[test]
    fn completed_output_receiver_enforces_exact_and_over_limit_boundaries() {
        let limit = GIT_CHECK_OUTPUT_BYTES;
        assert_eq!(
            validate_output_size(vec![0; limit], limit).unwrap().len(),
            limit
        );
        assert_eq!(
            validate_output_size(vec![0; limit + 1], limit),
            Err(GitCheckFailure::ResponseTooLarge)
        );
    }

    #[test]
    fn selected_ssh_paths_are_literal_at_the_shell_boundary() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("unexpected-command");
        let ssh_directory = temp.path().join("ssh ; touch unexpected-command ; #");
        std::fs::create_dir(&ssh_directory).unwrap();
        std::fs::write(ssh_directory.join("config"), "Host *\n").unwrap();
        std::fs::write(ssh_directory.join("id_ed25519"), "fixture key path only").unwrap();
        let argv_file = temp.path().join("argv");
        let fake_ssh = temp.path().join("fake ssh ' command");
        std::fs::write(
            &fake_ssh,
            "#!/bin/sh\nprintf '%s\\0' \"$@\" > \"$SSH_TEST_ARGV\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let command = ssh_command(&fake_ssh, &ssh_directory).unwrap();
        let status = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("{command} code.example git-upload-pack"))
            .env("SSH_TEST_ARGV", &argv_file)
            .current_dir(temp.path())
            .status()
            .unwrap();
        assert!(status.success());
        assert!(!marker.exists());
        let argv = std::fs::read(argv_file).unwrap();
        let args = argv
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8(arg.to_vec()).unwrap())
            .collect::<Vec<_>>();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-F", ssh_directory.join("config").to_str().unwrap()])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-i", ssh_directory.join("id_ed25519").to_str().unwrap()])
        );
        assert!(args.iter().any(|arg| arg
            == &format!(
                "UserKnownHostsFile=\"{}\"",
                ssh_directory.join("known_hosts").display()
            )));
        for option in [
            "BatchMode=yes",
            "StrictHostKeyChecking=yes",
            "IdentityAgent=none",
            "IdentityFile=none",
            "IdentitiesOnly=yes",
        ] {
            assert!(args.iter().any(|arg| arg == option));
        }
    }

    #[test]
    fn guest_git_configuration_cannot_override_selected_ssh_command() {
        let temp = tempfile::tempdir().unwrap();
        let mut git = GitContext::new(temp.path()).unwrap();
        git.ssh_command = Some("authorized-ssh".into());
        let command = git.command(
            temp.path(),
            &strings(&["ls-remote", "ssh://git@code.example/team/repo.git"]),
            None,
        );
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert!(
            args.iter()
                .any(|arg| arg == "core.sshCommand=authorized-ssh")
        );
        assert!(args.iter().any(|arg| arg == "protocol.ssh.allow=always"));
        assert!(
            !command
                .get_envs()
                .any(|(key, value)| key == "SSH_AUTH_SOCK" && value.is_some())
        );
        let git = GitContext::new(temp.path()).unwrap();
        let command = git.command(
            temp.path(),
            &strings(&["ls-remote", "ssh://git@code.example/team/repo.git"]),
            None,
        );
        assert!(
            !command
                .get_args()
                .any(|arg| arg == "protocol.ssh.allow=always")
        );
    }
    #[test]
    fn ssh_option_parser_preserves_supported_paths_and_rejects_expansion_syntax() {
        let temp = tempfile::tempdir().unwrap();
        for suffix in [
            "percent%h",
            "dollar${HOME}",
            "single'quote",
            "double\"quote",
            "back\\\\slash",
        ] {
            let directory = temp.path().join(suffix);
            std::fs::create_dir(&directory).unwrap();
            assert!(
                crate::validate_ssh_directory(&directory).is_err(),
                "must reject {suffix}"
            );
        }
        let directory = temp.path().join("selected ssh settings");
        std::fs::create_dir(&directory).unwrap();
        let command = ssh_command(Path::new("/usr/bin/ssh"), &directory).unwrap();
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("{command} -G example.test"))
            .output()
            .unwrap();
        assert!(output.status.success());
        let output = String::from_utf8(output.stdout).unwrap();
        let expected = format!(
            "userknownhostsfile {}",
            directory.join("known_hosts").display()
        );
        assert!(output.lines().any(|line| line == expected));
    }
}
