//! Spawn a child process on a pseudo-terminal: openpty on Unix, ConPTY on
//! Windows. A thin layer over `portable-pty` so the rest of jet never sees its
//! types and the backend can be swapped for our own ConPTY wrapper later.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// What to run in the PTY.
#[derive(Clone, Debug, Default)]
pub struct Command {
    /// Program to run; `None` runs the user's default shell.
    pub program: Option<OsString>,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(OsString, OsString)>,
}

impl Command {
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: Some(program.into()),
            ..Self::default()
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    fn to_builder(&self) -> CommandBuilder {
        let mut builder = match &self.program {
            Some(program) => {
                let mut b = CommandBuilder::new(program);
                b.args(&self.args);
                b
            }
            None => CommandBuilder::new_default_prog(),
        };
        if let Some(cwd) = &self.cwd {
            builder.cwd(cwd);
        }
        builder.env("TERM", "xterm-256color");
        builder.env("COLORTERM", "truecolor");
        // Apps opened from Finder get no locale, and without one shells
        // and programs fall back to ASCII.
        #[cfg(target_os = "macos")]
        if ["LANG", "LC_ALL", "LC_CTYPE"]
            .iter()
            .all(|v| std::env::var_os(v).is_none())
        {
            builder.env("LC_CTYPE", "UTF-8");
        }
        for (key, value) in &self.env {
            builder.env(key, value);
        }
        builder
    }
}

/// The working directory of the shell started as `pid`, if it can be read,
/// for opening a new one in the same place. On Windows that's the newest
/// process under it: launchers like Git Bash's `bin\bash.exe` run the real
/// shell as a child.
pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        windows_proc::cwd(pid)
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }
    #[cfg(target_os = "macos")]
    {
        mac_proc::cwd(pid)
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// The program in front in the shell started as `pid`: the newest process
/// under it, or the shell itself. Says what's running (`ssh.exe`) and how
/// (`ssh me@box`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    /// The executable's file name, e.g. `ssh.exe`.
    pub name: String,
    /// As passed to it, unsplit (Windows has no argv until the program
    /// parses one).
    pub command_line: String,
}

pub fn foreground(pid: u32) -> Option<Process> {
    #[cfg(windows)]
    {
        windows_proc::foreground(pid)
    }
    #[cfg(target_os = "linux")]
    {
        linux_proc::foreground(pid)
    }
    #[cfg(target_os = "macos")]
    {
        mac_proc::foreground(pid)
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// Linux says which process group has the terminal: the job in front, as
/// the shell's `/proc/<pid>/stat` reports it.
#[cfg(target_os = "linux")]
mod linux_proc {
    use crate::Process;

    pub fn foreground(pid: u32) -> Option<Process> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let front = tpgid(&stat).unwrap_or(pid);
        let name = std::fs::read_to_string(format!("/proc/{front}/comm")).ok()?;
        let argv = std::fs::read(format!("/proc/{front}/cmdline")).ok()?;
        let command_line = argv
            .split(|&b| b == 0)
            .filter(|arg| !arg.is_empty())
            .map(String::from_utf8_lossy)
            .collect::<Vec<_>>()
            .join(" ");
        Some(Process {
            pid: front,
            name: name.trim_end().to_owned(),
            command_line,
        })
    }

    /// The terminal's foreground process group, from a `stat` line: the
    /// sixth field after the command name, which is in parentheses and may
    /// hold anything.
    pub fn tpgid(stat: &str) -> Option<u32> {
        let rest = stat.get(stat.rfind(')')? + 1..)?;
        let field: i64 = rest.split_whitespace().nth(5)?.parse().ok()?;
        u32::try_from(field).ok().filter(|&g| g > 0)
    }
}

/// macOS answers for any process of the same user through `proc_pidinfo`
/// (the terminal's foreground group, the working directory) and the
/// `KERN_PROCARGS2` sysctl (the arguments).
#[cfg(target_os = "macos")]
mod mac_proc {
    use std::ffi::{OsString, c_char, c_int, c_void};
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;

    use crate::Process;

    pub fn foreground(pid: u32) -> Option<Process> {
        let shell: libc::proc_bsdinfo = info(pid, libc::PROC_PIDTBSDINFO)?;
        let front = match shell.e_tpgid {
            0 => pid,
            group => group,
        };
        let bsd: libc::proc_bsdinfo = info(front, libc::PROC_PIDTBSDINFO)?;
        let name = Some(text(&bsd.pbi_name))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| text(&bsd.pbi_comm));
        let command_line = args(front).map_or_else(|| name.clone(), |args| args.join(" "));
        Some(Process {
            pid: front,
            name,
            command_line,
        })
    }

    pub fn cwd(pid: u32) -> Option<PathBuf> {
        let info: libc::proc_vnodepathinfo = info(pid, libc::PROC_PIDVNODEPATHINFO)?;
        let path: Vec<u8> = info
            .pvi_cdir
            .vip_path
            .as_flattened()
            .iter()
            .map(|&c| c as u8)
            .take_while(|&b| b != 0)
            .collect();
        (!path.is_empty()).then(|| OsString::from_vec(path).into())
    }

    /// One `proc_pidinfo` flavor's answer about `pid`.
    fn info<T>(pid: u32, flavor: c_int) -> Option<T> {
        let pid = c_int::try_from(pid).ok()?;
        let size = c_int::try_from(size_of::<T>()).ok()?;
        let mut out = std::mem::MaybeUninit::<T>::zeroed();
        // SAFETY: proc_pidinfo writes at most `size` bytes into `out`.
        let n =
            unsafe { libc::proc_pidinfo(pid, flavor, 0, out.as_mut_ptr().cast::<c_void>(), size) };
        // SAFETY: the flavors used are plain C structs, valid all-zero and
        // filled in full when the whole size comes back.
        (n == size).then(|| unsafe { out.assume_init() })
    }

    fn text(chars: &[c_char]) -> String {
        let bytes: Vec<u8> = chars
            .iter()
            .map(|&c| c as u8)
            .take_while(|&b| b != 0)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn args(pid: u32) -> Option<Vec<String>> {
        let mut mib = [
            libc::CTL_KERN,
            libc::KERN_PROCARGS2,
            c_int::try_from(pid).ok()?,
        ];
        let mut size = 0usize;
        let null = std::ptr::null_mut();
        // SAFETY: a null buffer asks only for the size.
        if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, null, &raw mut size, null, 0) } != 0 {
            return None;
        }
        let mut buf = vec![0u8; size];
        // SAFETY: `buf` holds `size` bytes, and sysctl writes no more.
        let at = buf.as_mut_ptr().cast::<c_void>();
        if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, at, &raw mut size, null, 0) } != 0 {
            return None;
        }
        buf.truncate(size);
        procargs(&buf)
    }

    /// `KERN_PROCARGS2`'s layout: argc, the executable's path, NUL padding,
    /// then the arguments (and the environment after them).
    pub fn procargs(buf: &[u8]) -> Option<Vec<String>> {
        let (argc, rest) = buf.split_first_chunk::<4>()?;
        let argc = usize::try_from(i32::from_ne_bytes(*argc)).ok()?;
        let mut parts = rest.split(|&b| b == 0).filter(|p| !p.is_empty());
        parts.next()?;
        Some(
            parts
                .take(argc)
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .collect(),
        )
    }
}

/// Windows keeps a process's working directory in its PEB, which only that
/// process reads; another process with VM read access can copy it out. The
/// offsets are the 64-bit layouts of `PEB` and `RTL_USER_PROCESS_PARAMETERS`.
/// Which process: the newest descendant, as WezTerm picks the foreground one.
#[cfg(all(windows, target_pointer_width = "64"))]
mod windows_proc {
    use std::ffi::c_void;
    use std::path::PathBuf;

    use crate::Process;

    use windows::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_VM_READ,
    };

    /// `PEB.ProcessParameters`.
    const PROCESS_PARAMETERS: usize = 0x20;
    /// `RTL_USER_PROCESS_PARAMETERS.CurrentDirectory.DosPath`.
    const CURRENT_DIRECTORY: usize = 0x38;
    /// `RTL_USER_PROCESS_PARAMETERS.CommandLine`.
    const COMMAND_LINE: usize = 0x70;

    /// A Toolhelp snapshot entry.
    struct Entry {
        pid: u32,
        parent: u32,
        name: String,
    }

    /// `PROCESS_BASIC_INFORMATION`.
    #[repr(C)]
    #[derive(Default)]
    struct BasicInfo {
        exit_status: i32,
        peb: usize,
        affinity_mask: usize,
        base_priority: i32,
        pid: usize,
        parent_pid: usize,
    }

    pub fn cwd(pid: u32) -> Option<PathBuf> {
        // The newest may be gone already (a prompt's helper); the shell isn't.
        let all = processes()?;
        let path = |pid| read_string(pid, CURRENT_DIRECTORY);
        let dir = newest_descendant(&all, pid)
            .and_then(path)
            .or_else(|| path(pid))?;
        // Stored with a trailing backslash; a drive root keeps its own.
        let dir = match dir.trim_end_matches('\\') {
            root if root.ends_with(':') => dir.as_str(),
            trimmed => trimmed,
        };
        Some(PathBuf::from(dir))
    }

    pub fn foreground(pid: u32) -> Option<Process> {
        let all = processes()?;
        let front = newest_descendant(&all, pid)?;
        let name = all.iter().find(|e| e.pid == front)?.name.clone();
        Some(Process {
            pid: front,
            name,
            command_line: read_string(front, COMMAND_LINE).unwrap_or_default(),
        })
    }

    /// Down from `root`, always to the newest child, while there is one.
    /// Children must be younger than their parent: pids get reused.
    fn newest_descendant(all: &[Entry], root: u32) -> Option<u32> {
        let (mut at, mut born) = (root, created(root)?);
        // Ages strictly increase, so this can't loop; the bound is cheap.
        for _ in 0..all.len() {
            let child = all
                .iter()
                .filter(|e| e.parent == at)
                .filter_map(|e| Some((created(e.pid)?, e.pid)))
                .filter(|(time, _)| *time > born)
                .max();
            let Some((time, pid)) = child else {
                break;
            };
            (at, born) = (pid, time);
        }
        Some(at)
    }

    fn processes() -> Option<Vec<Entry>> {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?;
        let mut entry = PROCESSENTRY32W {
            dwSize: u32::try_from(size_of::<PROCESSENTRY32W>()).ok()?,
            ..Default::default()
        };
        let mut out = Vec::new();
        let mut more = unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok();
        while more {
            let name = &entry.szExeFile;
            let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            out.push(Entry {
                pid: entry.th32ProcessID,
                parent: entry.th32ParentProcessID,
                name: String::from_utf16_lossy(name.get(..len).unwrap_or_default()),
            });
            more = unsafe { Process32NextW(snapshot, &mut entry) }.is_ok();
        }
        let _ = unsafe { CloseHandle(snapshot) };
        Some(out)
    }

    /// When `pid` started, in 100ns ticks.
    fn created(pid: u32) -> Option<u64> {
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
        let mut times = [FILETIME::default(); 4];
        let [created, exited, kernel, user] = &mut times;
        let ok = unsafe { GetProcessTimes(process, created, exited, kernel, user) }.is_ok();
        let _ = unsafe { CloseHandle(process) };
        ok.then(|| u64::from(created.dwHighDateTime) << 32 | u64::from(created.dwLowDateTime))
    }

    /// A `UNICODE_STRING` in `pid`'s process parameters.
    fn read_string(pid: u32, offset: usize) -> Option<String> {
        let process =
            unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid) }.ok()?;
        let text = read(process, offset);
        let _ = unsafe { CloseHandle(process) };
        text
    }

    fn read(process: HANDLE, offset: usize) -> Option<String> {
        let mut info = BasicInfo::default();
        let status = unsafe {
            NtQueryInformationProcess(
                process,
                ProcessBasicInformation,
                (&raw mut info).cast(),
                u32::try_from(size_of::<BasicInfo>()).ok()?,
                std::ptr::null_mut(),
            )
        };
        if status.0 < 0 {
            return None;
        }
        let copy = |address: usize, out: &mut [u8]| {
            let from = address as *const c_void;
            unsafe { ReadProcessMemory(process, from, out.as_mut_ptr().cast(), out.len(), None) }
                .ok()
        };
        let mut word = [0u8; 8];
        copy(info.peb + PROCESS_PARAMETERS, &mut word)?;
        // UNICODE_STRING: byte length, capacity, padding, buffer pointer.
        let mut string = [0u8; 16];
        copy(usize::from_le_bytes(word) + offset, &mut string)?;
        let [l0, l1, _, _, _, _, _, _, b @ ..] = string;
        let len = usize::from(u16::from_le_bytes([l0, l1]));
        if len == 0 {
            return None;
        }
        let mut bytes = vec![0u8; len];
        copy(usize::from_le_bytes(b), &mut bytes)?;
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| u16::from_le_bytes(pair))
            .collect();
        String::from_utf16(&units).ok()
    }
}

#[cfg(all(windows, not(target_pointer_width = "64")))]
mod windows_proc {
    pub fn cwd(_: u32) -> Option<std::path::PathBuf> {
        None
    }

    pub fn foreground(_: u32) -> Option<crate::Process> {
        None
    }
}

/// Terminal size in cells, plus the cell size in pixels for size reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

impl From<Size> for PtySize {
    fn from(size: Size) -> Self {
        Self {
            rows: size.rows,
            cols: size.cols,
            pixel_width: size.cols.saturating_mul(size.cell_width),
            pixel_height: size.rows.saturating_mul(size.cell_height),
        }
    }
}

/// The PTY side of a running child. Reading and writing go through the
/// handles from [`Pty::spawn`], and waiting through [`Child`], so each can
/// live on its own thread.
///
/// Dropping it closes the PTY. On Windows that is what ends reads: ConPTY
/// keeps its output pipe open after the child exits until the pseudoconsole
/// is closed, so wait on the [`Child`], not for EOF.
pub struct Pty {
    master: Box<dyn MasterPty + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    pid: Option<u32>,
}

impl std::fmt::Debug for Pty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pty")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

/// The child process, for waiting on its exit.
pub struct Child(Box<dyn portable_pty::Child + Send + Sync>);

impl Child {
    /// Block until the child exits and return its exit code.
    pub fn wait(&mut self) -> io::Result<u32> {
        self.0.wait().map(|status| status.exit_code())
    }
}

pub type Reader = Box<dyn Read + Send>;
pub type Writer = Box<dyn Write + Send>;

impl Pty {
    pub fn spawn(command: &Command, size: Size) -> io::Result<(Self, Child, Reader, Writer)> {
        let pair = native_pty_system()
            .openpty(size.into())
            .map_err(io::Error::other)?;
        let child = pair
            .slave
            .spawn_command(command.to_builder())
            .map_err(io::Error::other)?;
        // Only the child may hold the slave side, or reads never see EOF
        // after it exits.
        drop(pair.slave);
        let reader = pair.master.try_clone_reader().map_err(io::Error::other)?;
        let writer = pair.master.take_writer().map_err(io::Error::other)?;
        let pty = Self {
            master: pair.master,
            killer: child.clone_killer(),
            pid: child.process_id(),
        };
        Ok((pty, Child(child), reader, writer))
    }

    pub fn resize(&self, size: Size) -> io::Result<()> {
        self.master.resize(size.into()).map_err(io::Error::other)
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn kill(&mut self) -> io::Result<()> {
        self.killer.kill()
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use crate::linux_proc::tpgid;

    #[test]
    fn foreground_group_from_stat() {
        let stat = "4242 (my (odd) prog) S 1 4242 4242 34816 5150 4194560 0 0";
        assert_eq!(tpgid(stat), Some(5150));
        // No controlling terminal.
        assert_eq!(tpgid("1 (init) S 0 1 1 0 -1 4194560"), None);
    }
}

#[cfg(all(test, target_os = "macos"))]
mod mac_tests {
    use crate::mac_proc::procargs;

    #[test]
    fn arguments_follow_the_executable_path() {
        let mut buf = 2i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/usr/bin/ssh\0\0\0\0ssh\0me@box\0HOME=/Users/me\0");
        assert_eq!(
            procargs(&buf),
            Some(vec!["ssh".to_owned(), "me@box".to_owned()])
        );
        assert_eq!(procargs(&[1, 0]), None);
    }

    #[test]
    fn a_process_answers_about_itself() {
        let me = crate::foreground(std::process::id()).map(|p| p.name);
        assert!(me.is_some_and(|name| !name.is_empty()));
        assert!(crate::process_cwd(std::process::id()).is_some());
    }
}
