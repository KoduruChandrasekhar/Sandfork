//! `sandfork-spawner`: single-threaded, started by the daemon at boot.
//!
//! It receives spawn requests on descriptor 3 and does the `clone3`, the
//! user-namespace maps, the mounts and the `execve` of the shim. The daemon
//! is multithreaded and async, so it never forks a sandbox itself: only the
//! calling thread survives a `fork`, locks held by other threads stay locked
//! forever, and `clone3(CLONE_NEWUSER)` fails with `EINVAL` in a threaded
//! process.

use sandfork_core::wire::{recv_frame, send_frame, SpawnReq, SpawnResp};
use sandfork_core::SfError;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

const CTL: i32 = 3;

fn reap() {
    // Sandboxes are this process's children. Their PID 1 is reaped here; the
    // daemon watches the same processes through pidfds.
    loop {
        // SAFETY: waitpid with a null status pointer is allowed.
        if unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) } <= 0 {
            break;
        }
    }
}

fn main() {
    if let Err(e) = sandfork_ns::assert_single_threaded("sandfork-spawner") {
        eprintln!("{e}");
        std::process::exit(2);
    }
    // SAFETY: prctl and signal-mask setup on a single-threaded process. The
    // spawner dies with the daemon, and every sandbox dies with the spawner.
    let sigfd = unsafe {
        libc::prctl(
            libc::PR_SET_PDEATHSIG,
            libc::SIGKILL as libc::c_ulong,
            0,
            0,
            0,
        );
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGCHLD);
        libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        OwnedFd::from_raw_fd(libc::signalfd(
            -1,
            &set,
            libc::SFD_CLOEXEC | libc::SFD_NONBLOCK,
        ))
    };
    // Compiled once; every child inherits the programs through fork.
    let seccomp = match sandfork_seccomp::compile(&sandfork_seccomp::policy()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("sandfork-spawner: compile seccomp policy: {e}");
            std::process::exit(2);
        }
    };

    loop {
        let mut pfds = [
            libc::pollfd {
                fd: CTL,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: sigfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `pfds` is a valid two-element array.
        if unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) } < 0 {
            continue;
        }
        if pfds[1].revents & libc::POLLIN != 0 {
            let mut buf = [0u8; 1024];
            // SAFETY: draining the signalfd into a live buffer.
            while unsafe { libc::read(sigfd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {
            }
            reap();
        }
        if pfds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) == 0 {
            continue;
        }
        let req = match recv_frame::<SpawnReq>(CTL) {
            Ok((r, _)) => r,
            Err(_) => std::process::exit(0), // the daemon went away
        };
        match req {
            SpawnReq::Ping => {
                let _ = send_frame(
                    CTL,
                    &SpawnResp::Pong {
                        threads: sandfork_ns::thread_count() as u32,
                    },
                    &[],
                );
            }
            SpawnReq::Shutdown => std::process::exit(0),
            SpawnReq::Spawn(cfg) => {
                let result = sandfork_core::fail::point("spawn")
                    .and_then(|_| sandfork_ns::spawn::spawn(&cfg, &seccomp));
                match result {
                    Ok(s) => {
                        let _ = send_frame(
                            CTL,
                            &SpawnResp::Spawned {
                                pid: s.pid,
                                steps: s.steps,
                            },
                            &[s.ctl.as_raw_fd(), s.pidfd.as_raw_fd()],
                        );
                    }
                    Err(e) => {
                        let _ = send_frame(
                            CTL,
                            &SpawnResp::Err(SfError::infra(&e.code, e.message)),
                            &[],
                        );
                    }
                }
                reap();
            }
        }
    }
}
