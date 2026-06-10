// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// NOTE: It may be worth integrating pwsh-install.ps1 and the template directly into this executable.
// This way it would be a lot more portable. It accepts a -Command on stdin, so it may be easy to do.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::os::windows::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process;
use std::ptr;

use clap::builder::NonEmptyStringValueParser;
use clap::{Arg, Command};
use uucore::Args;
use uucore::error::{FromIo as _, UError, UResult, USimpleError};
use windows_sys::Win32::Foundation;
use windows_sys::Win32::Security::Cryptography;
use windows_sys::Win32::Storage::FileSystem;
use windows_sys::Win32::System::Console;
use windows_sys::Win32::System::Pipes;
use windows_sys::Win32::System::Registry;
use windows_sys::Win32::UI::Shell;
use windows_sys::Win32::UI::WindowsAndMessaging;
use windows_sys::w;

#[uucore::main(no_signals)]
pub fn uumain<T: Args>(args: T) -> UResult<()> {
    let args = args.collect::<Vec<_>>();
    let _stdout_pipe = connect_stdout_pipe(&args)?;
    let matches = uucore::clap_localization::handle_clap_result_with_exit_code(
        uu_app(),
        args.clone().into_iter(),
        2,
    )?;
    let utilities = utility_names();
    let mut disabled = read_disabled_aliases()?;

    match matches.subcommand() {
        Some((action @ ("enable" | "disable"), matches)) => {
            let names: Vec<_> = matches
                .get_many::<String>("utility")
                .expect("utility is a required parameter")
                .map(String::as_str)
                .collect();

            for name in names {
                if !utilities.contains(name) {
                    return Err(USimpleError::new(
                        1,
                        format!("unknown coreutils utility: {name}"),
                    ));
                }
            }

            if !ensure_elevated(action, &utilities, matches.get_flag("no-elevate"))? {
                return Ok(());
            }

            for &utility_name in &utilities {
                if action == "enable" {
                    disabled.remove(utility_name);
                } else {
                    disabled.insert(utility_name.to_string());
                }
            }
            write_disabled_aliases(&disabled)?;
            sync_install(&utilities, &disabled)
        }
        Some(("refresh", _)) => sync_install(&utilities, &disabled),
        Some(("status", _)) => {
            for utility_name in utilities {
                let status = if disabled.contains(utility_name) {
                    "disabled"
                } else {
                    "enabled"
                };
                println!("{utility_name:16}{status}");
            }
            Ok(())
        }
        _ => unreachable!("clap enforces a known subcommand"),
    }
}

pub fn uu_app() -> Command {
    Command::new("coreutils-manager")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Manage coreutils utilities and PowerShell profiles")
        .arg(
            Arg::new("no-elevate")
                .long("no-elevate")
                .hide(true)
                .global(true)
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            Arg::new("stdout-pipe")
                .long("stdout-pipe")
                .hide(true)
                .global(true)
                .value_name("PATH")
                .value_parser(NonEmptyStringValueParser::new()),
        )
        .subcommand_required(true)
        .subcommand(
            Command::new("enable")
                .about("Enable one or more utilities")
                .arg(
                    Arg::new("utility")
                        .help("Utility names to enable")
                        .num_args(1..)
                        .required(true)
                        .trailing_var_arg(true)
                        .value_parser(NonEmptyStringValueParser::new()),
                ),
        )
        .subcommand(
            Command::new("disable")
                .about("Disable one or more utilities")
                .arg(
                    Arg::new("utility")
                        .help("Utility names to disable")
                        .num_args(1..)
                        .required(true)
                        .trailing_var_arg(true)
                        .value_parser(NonEmptyStringValueParser::new()),
                ),
        )
        .subcommand(Command::new("refresh").about("Refresh utility links and PowerShell profiles"))
        .subcommand(Command::new("status").about("List all utilities with their status"))
}

fn utility_names() -> BTreeSet<&'static str> {
    let mut result = crate::utility_names()
        .iter()
        .copied()
        .filter(|&utility_name| utility_name != "[" && utility_name != "coreutils-manager")
        .collect::<BTreeSet<_>>();
    if result.contains("ls") {
        result.insert("la");
    }
    result
}

fn connect_stdout_pipe(args: &[OsString]) -> UResult<Option<OwnedHandle>> {
    let Some(pipe_path) = stdout_pipe_arg(args)? else {
        return Ok(None);
    };

    let pipe_path = wide_null(OsStr::new(&pipe_path));
    let handle = unsafe {
        FileSystem::CreateFileW(
            pipe_path.as_ptr(),
            FileSystem::FILE_GENERIC_WRITE,
            0,
            ptr::null(),
            FileSystem::OPEN_EXISTING,
            FileSystem::FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if handle == Foundation::INVALID_HANDLE_VALUE {
        return Err(last_os_error("failed to connect stdout pipe"));
    }

    if unsafe { Console::SetStdHandle(Console::STD_OUTPUT_HANDLE, handle) } == 0
        || unsafe { Console::SetStdHandle(Console::STD_ERROR_HANDLE, handle) } == 0
    {
        unsafe {
            Foundation::CloseHandle(handle);
        }
        return Err(last_os_error("failed to redirect stdout/stderr"));
    }

    Ok(Some(OwnedHandle(handle)))
}

fn stdout_pipe_arg(args: &[OsString]) -> UResult<Option<String>> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let Some(arg) = arg.to_str() else {
            continue;
        };
        if arg == "--stdout-pipe" {
            let Some(path) = args.next() else {
                return Err(USimpleError::new(
                    2,
                    "--stdout-pipe requires a pipe path".to_string(),
                ));
            };
            return path
                .to_str()
                .map(|path| Some(path.to_string()))
                .ok_or_else(|| {
                    USimpleError::new(2, "--stdout-pipe path is not valid Unicode".to_string())
                });
        }
        if let Some(path) = arg.strip_prefix("--stdout-pipe=") {
            return Ok(Some(path.to_string()));
        }
    }

    Ok(None)
}

fn ensure_elevated(
    action: &str,
    utility_names: &BTreeSet<&str>,
    no_elevate: bool,
) -> UResult<bool> {
    if is_elevated() {
        return Ok(true);
    }
    if no_elevate {
        return Err(USimpleError::new(
            1,
            "administrator privileges are required".to_string(),
        ));
    }

    elevate(action, utility_names)?;
    Ok(false)
}

fn is_elevated() -> bool {
    unsafe { Shell::IsUserAnAdmin() != 0 }
}

fn elevate(action: &str, utility_names: &BTreeSet<&str>) -> UResult<()> {
    let pipe_path = random_pipe_path()?;
    let pipe = NamedPipe::create(&pipe_path)?;
    let exe = std::env::current_exe()?;

    let mut parameters = Vec::new();
    if !exe
        .file_stem()
        .is_some_and(|stem| stem == "coreutils-manager")
    {
        parameters.push("coreutils-manager".to_string());
    }
    parameters.push("--no-elevate".to_string());
    parameters.push("--stdout-pipe".to_string());
    parameters.push(pipe_path.to_string());
    parameters.push(action.to_string());
    parameters.extend(utility_names.iter().map(|&name| name.to_string()));

    let parameters = parameters
        .iter()
        .map(|arg| quote_windows_arg(arg))
        .collect::<Vec<_>>()
        .join(" ");

    let exe = wide_null(exe.as_os_str());
    let parameters = wide_null(OsStr::new(&parameters));
    let result = unsafe {
        Shell::ShellExecuteW(
            ptr::null_mut(),
            w!("runas"),
            exe.as_ptr(),
            parameters.as_ptr(),
            ptr::null(),
            WindowsAndMessaging::SW_NORMAL,
        )
    };
    if result as usize <= 32 {
        return Err(USimpleError::new(
            1,
            format!("failed to elevate coreutils-manager: {}", result as usize),
        ));
    }

    pipe.connect()?;
    pipe.copy_to_stdout()
}

fn random_pipe_path() -> UResult<String> {
    let mut random = [0u8; 16];
    unsafe { Cryptography::ProcessPrng(random.as_mut_ptr(), random.len()) };

    let mut suffix = String::with_capacity(random.len() * 2);
    for byte in random {
        _ = write!(suffix, "{byte:02x}");
    }
    Ok(format!(
        r"\\.\pipe\coreutils-manager-{}-{suffix}",
        std::process::id()
    ))
}

fn quote_windows_arg(arg: &str) -> String {
    if !arg.is_empty()
        && !arg
            .bytes()
            .any(|byte| byte == b' ' || byte == b'\t' || byte == b'"')
    {
        return arg.to_string();
    }

    let mut quoted = String::from("\"");
    let mut backslashes = 0;
    for ch in arg.chars() {
        if ch == '\\' {
            backslashes += 1;
        } else if ch == '"' {
            quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
            quoted.push('"');
            backslashes = 0;
        } else {
            quoted.push_str(&"\\".repeat(backslashes));
            quoted.push(ch);
            backslashes = 0;
        }
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

struct NamedPipe(OwnedHandle);

impl NamedPipe {
    fn create(path: &str) -> UResult<Self> {
        let path = wide_null(OsStr::new(path));
        let handle = unsafe {
            Pipes::CreateNamedPipeW(
                path.as_ptr(),
                FileSystem::PIPE_ACCESS_INBOUND,
                Pipes::PIPE_TYPE_BYTE | Pipes::PIPE_WAIT,
                1,
                16 * 1024,
                16 * 1024,
                0,
                ptr::null(),
            )
        };
        if handle == Foundation::INVALID_HANDLE_VALUE {
            return Err(last_os_error("failed to create stdout pipe"));
        }
        Ok(Self(OwnedHandle(handle)))
    }

    fn connect(&self) -> UResult<()> {
        if unsafe { Pipes::ConnectNamedPipe(self.0.get(), ptr::null_mut()) } != 0 {
            return Ok(());
        }

        let error = unsafe { Foundation::GetLastError() };
        if error == Foundation::ERROR_PIPE_CONNECTED {
            return Ok(());
        }
        Err(USimpleError::new(
            1,
            format!(
                "failed to connect stdout pipe: {}",
                io::Error::from_raw_os_error(error as i32)
            ),
        ))
    }

    fn copy_to_stdout(&self) -> UResult<()> {
        let mut stdout = io::stdout().lock();
        let mut buffer = [0u8; 8192];
        loop {
            let mut read = 0u32;
            if unsafe {
                FileSystem::ReadFile(
                    self.0.get(),
                    buffer.as_mut_ptr(),
                    buffer.len() as u32,
                    &mut read,
                    ptr::null_mut(),
                )
            } == 0
            {
                let error = unsafe { Foundation::GetLastError() };
                if error == Foundation::ERROR_BROKEN_PIPE {
                    stdout.flush().map_err(|err| {
                        USimpleError::new(1, format!("failed to flush stdout: {err}"))
                    })?;
                    return Ok(());
                }
                return Err(USimpleError::new(
                    1,
                    format!(
                        "failed to read stdout pipe: {}",
                        io::Error::from_raw_os_error(error as i32)
                    ),
                ));
            }

            if read == 0 {
                return Ok(());
            }
            stdout.write_all(&buffer[..read as usize]).map_err(|err| {
                USimpleError::new(1, format!("failed to write child output: {err}"))
            })?;
        }
    }
}

fn sync_install(utility_names: &BTreeSet<&str>, disabled: &BTreeSet<String>) -> UResult<()> {
    let app_dir = app_dir()?;
    let bin_dir = app_dir.join("bin");
    let cmd_dir = app_dir.join("cmd");
    let coreutils_exe = app_dir.join("coreutils.exe");

    sync_alias_links(utility_names, disabled, &bin_dir, &cmd_dir, &coreutils_exe)?;
    refresh_powershell_profiles(&app_dir, &cmd_dir)?;
    Ok(())
}

fn app_dir() -> UResult<PathBuf> {
    let exe = std::env::current_exe()
        .map_err(|err| USimpleError::new(1, format!("current_exe failed: {err}")))?;
    let parent = exe.parent().ok_or_else(|| {
        USimpleError::new(
            1,
            format!("cannot find parent directory of {}", exe.display()),
        )
    })?;
    if parent
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("bin") || name.eq_ignore_ascii_case("cmd"))
    {
        return parent.parent().map(Path::to_path_buf).ok_or_else(|| {
            USimpleError::new(
                1,
                format!("cannot find install directory of {}", exe.display()),
            )
        });
    }
    Ok(parent.to_path_buf())
}

fn sync_alias_links(
    utility_names: &BTreeSet<&str>,
    disabled: &BTreeSet<String>,
    bin_dir: &Path,
    cmd_dir: &Path,
    coreutils_exe: &Path,
) -> UResult<()> {
    fs::create_dir_all(bin_dir)?;
    fs::create_dir_all(cmd_dir)?;

    for &utility_name in utility_names {
        if utility_name == "la" {
            continue;
        }

        sync_alias_link(
            &bin_dir.join(format!("{utility_name}.exe")),
            coreutils_exe,
            disabled.contains(utility_name),
        )?;
        sync_alias_link(
            &cmd_dir.join(format!("{utility_name}.cmd")),
            coreutils_exe,
            disabled.contains(utility_name),
        )?;
    }

    sync_alias_link(&bin_dir.join("coreutils-manager.exe"), coreutils_exe, false)?;
    remove_file_if_exists(&cmd_dir.join("coreutils-manager.cmd"))
        .map_err_context(|| "failed to remove stale coreutils-manager.cmd".to_string())?;

    Ok(())
}

fn sync_alias_link(link: &Path, target: &Path, disabled: bool) -> UResult<()> {
    if disabled {
        remove_file_if_exists(link)?;
    } else if !link.exists() {
        fs::hard_link(target, link)?;
    }
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn refresh_powershell_profiles(app_dir: &Path, cmd_dir: &Path) -> UResult<()> {
    let script = app_dir.join("pwsh-install.ps1");
    if !script.is_file() {
        return Ok(());
    }

    let status = match process::Command::new("pwsh.exe")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-File")
        .arg(&script)
        .arg("-Action")
        .arg("Refresh")
        .arg("-CmdDir")
        .arg(cmd_dir)
        .status()
    {
        Ok(status) => status,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(USimpleError::new(
                1,
                format!("failed to start pwsh.exe: {err}"),
            ));
        }
    };
    if !status.success() {
        return Err(USimpleError::new(
            1,
            format!("failed to refresh PowerShell profiles: {status}"),
        ));
    }
    Ok(())
}

fn read_disabled_aliases() -> UResult<BTreeSet<String>> {
    let mut bytes = 0u32;
    let ret = unsafe {
        Registry::RegGetValueW(
            Registry::HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\Microsoft\\coreutils"),
            w!("DisabledAliases"),
            Registry::RRF_RT_REG_MULTI_SZ,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut bytes,
        )
    };
    if ret == Foundation::ERROR_FILE_NOT_FOUND || ret == Foundation::ERROR_PATH_NOT_FOUND {
        return Ok(BTreeSet::new());
    }
    reg_check_result("failed to read disabled aliases from registry", ret)?;
    if bytes == 0 {
        return Ok(BTreeSet::new());
    }

    let mut data = vec![0u16; bytes as usize / 2];
    reg_check_result("failed to read disabled aliases from registry", unsafe {
        Registry::RegGetValueW(
            Registry::HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\Microsoft\\coreutils"),
            w!("DisabledAliases"),
            Registry::RRF_RT_REG_MULTI_SZ,
            ptr::null_mut(),
            data.as_mut_ptr().cast(),
            &mut bytes,
        )
    })?;

    Ok(parse_multi_sz(&data))
}

fn parse_multi_sz(data: &[u16]) -> BTreeSet<String> {
    let mut result = BTreeSet::new();
    let mut start = 0;
    for (index, &ch) in data.iter().enumerate() {
        if ch != 0 {
            continue;
        }
        if index == start {
            break;
        }
        let alias = String::from_utf16_lossy(&data[start..index]).to_ascii_lowercase();
        if !alias.is_empty() {
            result.insert(alias);
        }
        start = index + 1;
    }
    result
}

fn write_disabled_aliases(disabled: &BTreeSet<String>) -> UResult<()> {
    let key = OwnedHKEY::create_key(
        Registry::HKEY_LOCAL_MACHINE,
        r"SOFTWARE\Microsoft\coreutils",
    )?;
    if disabled.is_empty() {
        let ret = unsafe { Registry::RegDeleteValueW(key.get(), w!("DisabledAliases")) };
        if ret != 0 && ret != Foundation::ERROR_FILE_NOT_FOUND {
            return Err(USimpleError::new(
                1,
                format!("failed to delete disabled aliases registry value: {ret}"),
            ));
        }
        return Ok(());
    }

    let values = disabled.iter().cloned().collect::<Vec<_>>();
    let data = make_multi_sz(&values);
    reg_check_result("failed to write disabled aliases to registry", unsafe {
        Registry::RegSetValueExW(
            key.get(),
            w!("DisabledAliases"),
            0,
            Registry::REG_MULTI_SZ,
            data.as_ptr().cast(),
            (data.len() * size_of::<u16>()) as u32,
        )
    })?;
    Ok(())
}

fn make_multi_sz(values: &[String]) -> Vec<u16> {
    let mut data = Vec::new();
    for value in values {
        data.extend(value.encode_utf16());
        data.push(0);
    }
    data.push(0);
    data
}

struct OwnedHandle(Foundation::HANDLE);

impl OwnedHandle {
    fn get(&self) -> Foundation::HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != Foundation::INVALID_HANDLE_VALUE {
            unsafe { Foundation::CloseHandle(self.0) };
        }
    }
}

struct OwnedHKEY(Registry::HKEY);

impl OwnedHKEY {
    fn create_key(key: Registry::HKEY, subkey: &str) -> UResult<Self> {
        let subkey = wide_null(OsStr::new(subkey));
        let mut result = ptr::null_mut();
        reg_check_result("failed to create registry key", unsafe {
            Registry::RegCreateKeyW(key, subkey.as_ptr(), &mut result)
        })?;
        Ok(Self(result))
    }

    fn get(&self) -> Registry::HKEY {
        self.0
    }
}

impl Drop for OwnedHKEY {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { Registry::RegCloseKey(self.0) };
        }
    }
}

fn wide_null(value: impl AsRef<OsStr>) -> Vec<u16> {
    value
        .as_ref()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn last_os_error(context: &str) -> Box<dyn UError> {
    io::Error::last_os_error().map_err_context(|| context.to_string())
}

fn reg_check_result(context: &str, result: u32) -> UResult<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(result as i32).map_err_context(|| context.to_string()))
    }
}
