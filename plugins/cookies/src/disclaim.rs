//! macOS: make this program its own *responsible process*, so the privacy checks (TCC) that guard other apps' files,
//! such as the browser's cookie database, ask about `fleetlayer-cookies` itself rather than the node that started it.
//! Without this, reading the cookies needs a grant (Full Disk Access) for the node, which would cover every plugin and
//! tool the node starts.
//!
//! How: at start, the program starts itself again through `posix_spawn` with the spawn attribute
//! `responsibility_spawnattrs_setdisclaim` set, waits for that copy, and exits with its status. The copy inherits stdin,
//! stdout and stderr, so it speaks the plugin protocol to the node directly. When the node stops the plugin (closing
//! its stdin, or killing the first process and dropping the pipes), the copy reads the end of its input and exits.
//!
//! `responsibility_spawnattrs_setdisclaim` is a private API that terminals and debuggers (LLDB, Qt Creator) use for
//! the same reason. It's looked up at run time: where it's missing, the program carries on without it and says so.
//! This module is the only unsafe code in the crate (foreign calls).
#![allow(unsafe_code)]

/// Set in the relaunched copy so it doesn't relaunch again.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const MARK: &str = "FLEETLAYER_COOKIES_DISCLAIMED";

/// On macOS, unless this is the relaunched copy: relaunch, wait and exit with the copy's status. Returns in the process
/// that should go on (the copy, any other system, or this one when relaunching isn't possible).
pub fn relaunch() {
    #[cfg(target_os = "macos")]
    imp::relaunch();
}

/// The pid of this process's responsible process, on macOS; None elsewhere or when the system can't say.
pub fn responsible_pid() -> Option<i32> {
    #[cfg(target_os = "macos")]
    {
        imp::responsible_pid()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{CString, c_char, c_int, c_void};
    use std::os::unix::ffi::OsStrExt;
    use std::ptr;

    type SetDisclaim = unsafe extern "C" fn(*mut libc::posix_spawnattr_t, c_int) -> c_int;
    type ResponsibleFor = unsafe extern "C" fn(libc::pid_t) -> libc::pid_t;

    fn symbol(name: &str) -> Option<*mut c_void> {
        let name = CString::new(name).ok()?;
        // SAFETY: dlsym with RTLD_DEFAULT and a NUL-terminated name only looks the symbol up.
        let f = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
        (!f.is_null()).then_some(f)
    }

    pub fn responsible_pid() -> Option<i32> {
        let f = symbol("responsibility_get_pid_responsible_for_pid")?;
        // SAFETY: the symbol has this signature (pid in, pid out) on every macOS that exports it.
        let f: ResponsibleFor = unsafe { std::mem::transmute::<*mut c_void, ResponsibleFor>(f) };
        // SAFETY: a plain call with our own pid.
        let pid = unsafe { f(libc::getpid()) };
        (pid > 0).then_some(pid)
    }

    pub fn relaunch() {
        if std::env::var_os(super::MARK).is_some() {
            return;
        }
        match spawn_and_wait() {
            Ok(code) => std::process::exit(code),
            Err(e) => eprintln!(
                "fleetlayer-cookies: couldn't start as its own responsible process ({e}); macOS will hold the node \
                 responsible for reading the browser's files"
            ),
        }
    }

    fn cstring(bytes: &[u8]) -> Result<CString, String> {
        CString::new(bytes).map_err(|_| "an argument or variable with a NUL byte".to_string())
    }

    fn spawn_and_wait() -> Result<i32, String> {
        let setdisclaim = symbol("responsibility_spawnattrs_setdisclaim")
            .ok_or("this macOS has no responsibility_spawnattrs_setdisclaim")?;
        // SAFETY: the symbol has this signature (attributes, flag) on every macOS that exports it.
        let setdisclaim: SetDisclaim = unsafe { std::mem::transmute::<*mut c_void, SetDisclaim>(setdisclaim) };
        let exe = std::env::current_exe().map_err(|e| format!("finding this program: {e}"))?;
        let path = cstring(exe.as_os_str().as_bytes())?;
        let args: Vec<CString> = std::env::args_os().map(|a| cstring(a.as_bytes())).collect::<Result<_, _>>()?;
        let mut env: Vec<CString> = std::env::vars_os()
            .map(|(k, v)| cstring(&[k.as_bytes(), b"=", v.as_bytes()].concat()))
            .collect::<Result<_, _>>()?;
        env.push(cstring(format!("{}=1", super::MARK).as_bytes())?);
        let mut argv: Vec<*mut c_char> = args.iter().map(|a| a.as_ptr() as *mut c_char).collect();
        argv.push(ptr::null_mut());
        let mut envp: Vec<*mut c_char> = env.iter().map(|e| e.as_ptr() as *mut c_char).collect();
        envp.push(ptr::null_mut());

        // SAFETY: the attributes are initialised before use and destroyed once; every pointer passed to posix_spawn
        // points into `path`, `args` and `env`, which outlive the call, and both arrays end with a null pointer.
        unsafe {
            let mut attr: libc::posix_spawnattr_t = ptr::null_mut();
            if libc::posix_spawnattr_init(&mut attr) != 0 {
                return Err("posix_spawnattr_init failed".into());
            }
            let r = setdisclaim(&mut attr, 1);
            if r != 0 {
                libc::posix_spawnattr_destroy(&mut attr);
                return Err(format!("responsibility_spawnattrs_setdisclaim answered {r}"));
            }
            let mut pid: libc::pid_t = 0;
            let r = libc::posix_spawn(&mut pid, path.as_ptr(), ptr::null(), &attr, argv.as_ptr(), envp.as_ptr());
            libc::posix_spawnattr_destroy(&mut attr);
            if r != 0 {
                return Err(format!("posix_spawn: {}", std::io::Error::from_raw_os_error(r)));
            }
            let mut status: c_int = 0;
            loop {
                if libc::waitpid(pid, &mut status, 0) == pid {
                    break;
                }
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::EINTR) {
                    // The copy is running and speaks for the plugin, so this process must never go on as well.
                    eprintln!("fleetlayer-cookies: waiting for the relaunched copy: {err}");
                    std::process::exit(1);
                }
            }
            Ok(if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) } else { 128 + libc::WTERMSIG(status) })
        }
    }
}
