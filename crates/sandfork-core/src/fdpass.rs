//! File-descriptor passing over Unix sockets (`SCM_RIGHTS`).
//!
//! Control sockets and pidfds cross process boundaries this way. A plain
//! `read(2)` silently discards ancillary data, so every channel that may carry
//! descriptors is read with [`recv_with_fds`].

use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

const MAX_FDS: usize = 8;

/// Send `data` with `fds` attached. Returns the number of data bytes sent.
/// The descriptors travel with the first byte.
pub fn send_with_fds(sock: RawFd, data: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    assert!(fds.len() <= MAX_FDS);
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut libc::c_void,
        iov_len: data.len(),
    };
    // SAFETY: CMSG_SPACE is a pure size computation.
    let space =
        unsafe { libc::CMSG_SPACE((MAX_FDS * std::mem::size_of::<RawFd>()) as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    // SAFETY: an all-zero msghdr is a valid "empty" value.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        // SAFETY: CMSG_LEN and CMSG_SPACE are pure size computations.
        let len =
            unsafe { libc::CMSG_LEN((fds.len() * std::mem::size_of::<RawFd>()) as u32) } as usize;
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen =
            unsafe { libc::CMSG_SPACE((fds.len() * std::mem::size_of::<RawFd>()) as u32) } as _;
        // SAFETY: msg_control points at `cbuf`, which is large enough for one
        // cmsghdr carrying `fds.len()` descriptors, so FIRSTHDR is non-null and
        // CMSG_DATA stays inside the buffer.
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = len as _;
            std::ptr::copy_nonoverlapping(
                fds.as_ptr() as *const u8,
                libc::CMSG_DATA(c),
                fds.len() * std::mem::size_of::<RawFd>(),
            );
        }
    }
    loop {
        // SAFETY: `msg` references live buffers for the duration of the call.
        let n = unsafe { libc::sendmsg(sock, &msg, libc::MSG_NOSIGNAL) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINTR) {
            return Err(e);
        }
    }
}

/// Receive up to `buf.len()` bytes plus any descriptors attached to them.
/// Received descriptors are close-on-exec.
pub fn recv_with_fds(sock: RawFd, buf: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // SAFETY: CMSG_SPACE is a pure size computation.
    let space =
        unsafe { libc::CMSG_SPACE((MAX_FDS * std::mem::size_of::<RawFd>()) as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    // SAFETY: an all-zero msghdr is a valid "empty" value.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    let n = loop {
        // SAFETY: `msg` references live, writable buffers.
        let n = unsafe { libc::recvmsg(sock, &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if n >= 0 {
            break n as usize;
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINTR) {
            return Err(e);
        }
    };
    let mut fds = Vec::new();
    // SAFETY: the kernel filled msg_control/msg_controllen; the CMSG_* macros
    // walk only inside that region. Each descriptor in an SCM_RIGHTS message
    // is a fresh fd owned by this process, so wrapping it in OwnedFd is sound.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let payload = (*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let count = payload / std::mem::size_of::<RawFd>();
                let data = libc::CMSG_DATA(c) as *const RawFd;
                for i in 0..count {
                    fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(i))));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    Ok((n, fds))
}

/// A connected `AF_UNIX` `SOCK_STREAM` pair, both ends close-on-exec.
pub fn socketpair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut sv = [0 as RawFd; 2];
    // SAFETY: `sv` is a valid two-element array for the kernel to fill.
    let r = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            sv.as_mut_ptr(),
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: socketpair returned two fresh descriptors that we now own.
    Ok(unsafe { (OwnedFd::from_raw_fd(sv[0]), OwnedFd::from_raw_fd(sv[1])) })
}

/// A pipe, both ends close-on-exec. Returns (read end, write end).
pub fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut p = [0 as RawFd; 2];
    // SAFETY: `p` is a valid two-element array for the kernel to fill.
    let r = unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe2 returned two fresh descriptors that we now own.
    Ok(unsafe { (OwnedFd::from_raw_fd(p[0]), OwnedFd::from_raw_fd(p[1])) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;

    #[test]
    fn passes_a_descriptor() {
        let (a, b) = socketpair().unwrap();
        let (r, w) = pipe().unwrap();
        assert_eq!(
            send_with_fds(a.as_raw_fd(), b"x", &[r.as_raw_fd()]).unwrap(),
            1
        );
        drop(r);
        let mut buf = [0u8; 8];
        let (n, fds) = recv_with_fds(b.as_raw_fd(), &mut buf).unwrap();
        assert_eq!((n, fds.len()), (1, 1));
        let mut wf = std::fs::File::from(w);
        wf.write_all(b"hello").unwrap();
        drop(wf);
        let mut got = String::new();
        std::fs::File::from(fds.into_iter().next().unwrap())
            .read_to_string(&mut got)
            .unwrap();
        assert_eq!(got, "hello");
    }
}
