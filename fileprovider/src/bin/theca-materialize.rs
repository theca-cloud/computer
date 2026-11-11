//! Runs a command with dataless files materializing (macOS): a shell
//! started by a service (CI's) has it off, and reading a drive's file not
//! downloaded fails there with "Resource deadlock avoided". A user's
//! Terminal has it on.

#[cfg(target_os = "macos")]
fn main() {
    use std::os::unix::process::CommandExt;
    // sys/resource.h (not in the libc crate).
    const IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES: libc::c_int = 3;
    const IOPOL_SCOPE_PROCESS: libc::c_int = 0;
    const IOPOL_MATERIALIZE_DATALESS_FILES_ON: libc::c_int = 2;
    unsafe extern "C" {
        fn setiopolicy_np(
            iotype: libc::c_int,
            scope: libc::c_int,
            policy: libc::c_int,
        ) -> libc::c_int;
    }
    let mut arguments = std::env::args_os().skip(1);
    let Some(program) = arguments.next() else {
        eprintln!("usage: theca-materialize <command> [arguments]");
        std::process::exit(2);
    };
    // SAFETY: a plain system call on this process (its exec inherits it).
    unsafe {
        setiopolicy_np(
            IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
            IOPOL_SCOPE_PROCESS,
            IOPOL_MATERIALIZE_DATALESS_FILES_ON,
        )
    };
    let error = std::process::Command::new(&program).args(arguments).exec();
    eprintln!("theca-materialize: {}: {error}", program.to_string_lossy());
    std::process::exit(1);
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("theca-materialize is for macOS");
}
