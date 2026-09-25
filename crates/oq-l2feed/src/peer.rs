//! Who is on the other end of a local socket, as the kernel says.
//!
//! Beside the signal handler for the same reason: std has no API for it
//! on stable, and this crate is where the workspace keeps its few libc
//! calls, each argued for. `oq-live`'s control port needs it and forbids
//! `unsafe` itself.

use std::os::unix::net::UnixStream;

/// The connecting process's uid, read from the kernel — not from anything
/// the peer sent, which it could make up.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
#[must_use]
pub fn uid(stream: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = libc::socklen_t::try_from(core::mem::size_of::<libc::ucred>()).ok()?;
    // SAFETY: a valid, open fd for the stream's lifetime, and an
    // out-buffer of exactly the size passed; getsockopt writes nothing
    // past it.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &raw mut len,
        )
    };
    (rc == 0).then_some(cred.uid)
}

/// The connecting process's uid, read from the kernel.
#[cfg(not(target_os = "linux"))]
#[allow(unsafe_code)]
#[must_use]
pub fn uid(stream: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: a valid, open fd and two out-parameters of the right type.
    let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &raw mut uid, &raw mut gid) };
    (rc == 0).then_some(uid)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_connection_to_ourselves_reports_our_own_uid() {
        use std::os::unix::fs::MetadataExt;
        let (a, _b) = std::os::unix::net::UnixStream::pair().expect("pair");
        let dir = std::env::temp_dir().join(format!("oq-peer-{}", std::process::id()));
        std::fs::write(&dir, "").expect("file");
        let me = std::fs::metadata(&dir).expect("meta").uid();
        let _ = std::fs::remove_file(&dir);
        assert_eq!(super::uid(&a), Some(me));
    }
}
