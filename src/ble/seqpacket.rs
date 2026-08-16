//! The SOCK_SEQPACKET file descriptors BlueZ hands out for `AcquireWrite` / `AcquireNotify`.
//!
//! One `write()` = one ATT Write Command; one `read()` = one notification. Writes must never
//! exceed `mtu - 3` bytes (the socket silently truncates). Non-blocking, driven by tokio's
//! `AsyncFd`.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};

use tokio::io::unix::AsyncFd;
use tokio::io::Interest;

pub struct SeqPacket {
    fd: AsyncFd<OwnedFd>,
    /// ATT MTU BlueZ reported for this channel.
    pub mtu: u16,
}

impl SeqPacket {
    pub fn new(fd: OwnedFd, mtu: u16) -> io::Result<Self> {
        set_nonblocking(&fd)?;
        Ok(SeqPacket {
            fd: AsyncFd::with_interest(fd, Interest::READABLE | Interest::WRITABLE)?,
            mtu,
        })
    }

    /// Largest payload we will put in one packet.
    pub fn max_payload(&self) -> usize {
        (self.mtu as usize).saturating_sub(3).max(1)
    }

    /// Send one packet (must be ≤ `max_payload()` bytes). Waits for the socket to drain on EAGAIN.
    pub async fn send(&self, pkt: &[u8]) -> io::Result<()> {
        if pkt.len() > self.max_payload() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("packet {} > mtu-3 {}", pkt.len(), self.max_payload()),
            ));
        }
        loop {
            let mut guard = self.fd.writable().await?;
            match guard.try_io(|inner| write_all_once(inner.get_ref(), pkt)) {
                Ok(res) => return res,
                Err(_would_block) => continue,
            }
        }
    }

    /// Receive one packet (AcquireNotify mode). Returns 0 bytes on hang-up (link dropped).
    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut guard = self.fd.readable().await?;
            match guard.try_io(|inner| read_once(inner.get_ref(), buf)) {
                Ok(res) => return res,
                Err(_would_block) => continue,
            }
        }
    }
}

fn set_nonblocking(fd: &OwnedFd) -> io::Result<()> {
    let raw = fd.as_raw_fd();
    // SAFETY: plain fcntl on a valid fd we own.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if flags & libc::O_NONBLOCK == 0 {
        // SAFETY: as above.
        let r = unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn write_all_once(fd: &OwnedFd, pkt: &[u8]) -> io::Result<()> {
    // SAFETY: valid fd, valid buffer.
    let n = unsafe {
        libc::write(
            fd.as_raw_fd(),
            pkt.as_ptr() as *const libc::c_void,
            pkt.len(),
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n as usize != pkt.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            format!("short packet write: {n} of {}", pkt.len()),
        ));
    }
    Ok(())
}

fn read_once(fd: &OwnedFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: valid fd, valid buffer.
    let n = unsafe {
        libc::read(
            fd.as_raw_fd(),
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd;

    fn pair() -> (OwnedFd, OwnedFd) {
        let mut fds = [0i32; 2];
        // SAFETY: socketpair fills two fds we take ownership of.
        let r = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        };
        assert_eq!(r, 0);
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    #[tokio::test]
    async fn round_trip_and_mtu_guard() {
        let (a, b) = pair();
        let tx = SeqPacket::new(a, 23).unwrap();
        let rx = SeqPacket::new(b, 23).unwrap();
        assert_eq!(tx.max_payload(), 20);
        tx.send(&[1u8; 20]).await.unwrap();
        tx.send(&[2u8; 5]).await.unwrap();
        let mut buf = [0u8; 64];
        let n = rx.recv(&mut buf).await.unwrap();
        assert_eq!(n, 20);
        assert_eq!(&buf[..20], &[1u8; 20]);
        let n = rx.recv(&mut buf).await.unwrap();
        assert_eq!(n, 5);
        assert!(
            tx.send(&[0u8; 21]).await.is_err(),
            "over mtu-3 must be refused"
        );
        drop(tx);
        let n = rx.recv(&mut buf).await.unwrap();
        assert_eq!(n, 0, "hang-up reads as 0");
    }
}
