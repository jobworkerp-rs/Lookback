use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use coding_agent_executor::{
    CapabilityDocument, Executor, ExecutorError, ExecutorErrorCode, PROTOCOL_VERSION,
    encode_error_response,
};

const MAX_REQUEST_BYTES: usize = 1024 * 1024;

fn main() -> ExitCode {
    if let Some(status) = coding_agent_executor::run_askpass_mode() {
        return ExitCode::from(status as u8);
    }
    match run() {
        Ok(status) => ExitCode::from(status),
        Err(error) => {
            eprintln!("coding-agent-executor: {}", error.code);
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<u8, ExecutorError> {
    let mut args = std::env::args_os().skip(1);
    let mut config = None;
    let mut request = None;
    let mut response = None;
    let mut stdio = false;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--version") if args.len() == 0 => {
                println!(
                    "coding-agent-executor {} protocol={}",
                    coding_agent_executor::EXECUTOR_VERSION,
                    PROTOCOL_VERSION
                );
                return Ok(0);
            }
            Some("--capabilities") if args.len() == 0 => {
                let document = serde_json::to_string(&CapabilityDocument::current())
                    .map_err(|_| ExecutorError::new(ExecutorErrorCode::InvalidRequest))?;
                println!("{document}");
                return Ok(0);
            }
            Some("--help") if args.len() == 0 => {
                println!(
                    "coding-agent-executor --version | --capabilities | --config FILE (--stdio | --request-file FILE --response-file FILE)"
                );
                return Ok(0);
            }
            Some("--config") => config = Some(PathBuf::from(args.next().ok_or_else(invalid_cli)?)),
            Some("--request-file") => {
                request = Some(PathBuf::from(args.next().ok_or_else(invalid_cli)?))
            }
            Some("--response-file") => {
                response = Some(PathBuf::from(args.next().ok_or_else(invalid_cli)?))
            }
            Some("--stdio") => stdio = true,
            _ => return Err(invalid_cli()),
        }
    }
    let config_path = config.ok_or_else(invalid_cli)?;
    let executor = load_executor(&config_path)?;
    match (stdio, request, response) {
        (true, None, None) => run_stdio(&executor),
        (false, Some(request_path), Some(response_path)) => {
            let output = match read_limited_file(&request_path) {
                Ok(input) => tokio_runtime()?.block_on(executor.handle_json(&input)),
                Err(error) => encode_error_response(error),
            };
            write_response_file(&response_path, &output)?;
            Ok(0)
        }
        _ => Err(invalid_cli()),
    }
}

fn run_stdio(executor: &Executor) -> Result<u8, ExecutorError> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let runtime = tokio_runtime()?;
    while let Some(input) = read_limited_line(&mut reader)? {
        let output = match input {
            InputLine::Request(input) => runtime.block_on(executor.handle_json(&input)),
            InputLine::TooLarge => {
                encode_error_response(ExecutorError::new(ExecutorErrorCode::ResponseTooLarge))
            }
        };
        let stdout = io::stdout();
        let mut writer = stdout.lock();
        writer
            .write_all(&output)
            .and_then(|_| writer.write_all(b"\n"))
            .and_then(|_| writer.flush())
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))?;
    }
    Ok(0)
}

enum InputLine {
    Request(Vec<u8>),
    TooLarge,
}

fn read_limited_line(reader: &mut impl Read) -> Result<Option<InputLine>, ExecutorError> {
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    let mut too_large = false;
    loop {
        let count = reader
            .read(&mut byte)
            .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))?;
        if count == 0 {
            return if line.is_empty() && !too_large {
                Ok(None)
            } else {
                Ok(Some(if too_large {
                    InputLine::TooLarge
                } else {
                    InputLine::Request(line)
                }))
            };
        }
        if byte[0] == b'\n' {
            if !too_large && line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(if too_large {
                InputLine::TooLarge
            } else {
                InputLine::Request(line)
            }));
        }
        if line.len() == MAX_REQUEST_BYTES {
            too_large = true;
            continue;
        }
        if !too_large {
            line.push(byte[0]);
        }
    }
}

fn load_executor(path: &Path) -> Result<Executor, ExecutorError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::InvalidConfiguration))?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_REQUEST_BYTES as u64
    {
        return Err(ExecutorError::new(ExecutorErrorCode::InvalidConfiguration));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o022 != 0 {
            return Err(ExecutorError::new(ExecutorErrorCode::InvalidConfiguration));
        }
    }
    let bytes = read_limited_file(path)?;
    Executor::from_config_json(&bytes)
}

fn read_limited_file(path: &Path) -> Result<Vec<u8>, ExecutorError> {
    let file = open_read_nofollow(path)
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::InvalidConfiguration))?;
    let mut result = Vec::new();
    file.take((MAX_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut result)
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))?;
    if result.len() > MAX_REQUEST_BYTES {
        return Err(ExecutorError::new(ExecutorErrorCode::ResponseTooLarge));
    }
    Ok(result)
}

fn write_response_file(path: &Path, bytes: &[u8]) -> Result<(), ExecutorError> {
    let parent = path
        .parent()
        .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::InvalidRequest))?;
    let name = path
        .file_name()
        .ok_or_else(|| ExecutorError::new(ExecutorErrorCode::InvalidRequest))?
        .to_string_lossy();
    let temp = parent.join(format!(".{name}.{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))?;
    fs::rename(temp, path).map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))
}

fn open_read_nofollow(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        OpenOptions::new().read(true).open(path)
    }
}

fn tokio_runtime() -> Result<tokio::runtime::Runtime, ExecutorError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| ExecutorError::new(ExecutorErrorCode::IoFailure))
}

fn invalid_cli() -> ExecutorError {
    ExecutorError::new(ExecutorErrorCode::InvalidRequest)
}
