//! SOCK_SEQPACKET media socket (server/receive side). Mirrors the clone-daemon
//! transport: each datagram is a JSON `DaemonMsg`, dmabuf fds via `SCM_RIGHTS`.

use std::io::{IoSlice, IoSliceMut};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use anyhow::{Context, Result, anyhow};
use nix::sys::socket::{
    AddressFamily, ControlMessage, ControlMessageOwned, MsgFlags, SockFlag, SockType, UnixAddr,
    accept, bind, listen, recvmsg, sendmsg, socket, Backlog,
};
use wire::socket::{DaemonMsg, ServerMsg};

const MAX_PACKET_BYTES: usize = 32 * 1024 * 1024;

pub struct Listener {
    fd: OwnedFd,
}

impl Listener {
    pub fn bind(path: &str) -> Result<Self> {
        let _ = std::fs::remove_file(path); // stale socket
        let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::empty(), None)
            .context("socket")?;
        let addr = UnixAddr::new(path).context("UnixAddr")?;
        bind(fd.as_raw_fd(), &addr).with_context(|| format!("bind {path}"))?;
        listen(&fd, Backlog::new(4).unwrap()).context("listen")?;
        // World-writable: the control-server binds this as root in its container, and
        // each clone-daemon connects as the uid-1000 clone user from a sibling container
        // over the shared sock volume. No Docker idmapping is involved, but that
        // root-vs-1000 split still needs write perm for the non-root uid.
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o777));
        Ok(Self { fd })
    }

    pub fn accept(&self) -> Result<Conn> {
        let fd = accept(self.fd.as_raw_fd()).context("accept")?;
        // SAFETY: accept() returns a fresh owned fd.
        Ok(Conn { fd: unsafe { OwnedFd::from_raw_fd(fd) } })
    }
}

pub struct Conn {
    fd: OwnedFd,
}

impl Conn {
    /// Receive one `DaemonMsg` + any dmabuf fds (as owned fds).
    pub fn recv(&self) -> Result<(DaemonMsg, Vec<OwnedFd>)> {
        let packet_len = recv_packet_len(self.fd.as_raw_fd())?;
        let mut buf = vec![0u8; packet_len];
        let mut iov = [IoSliceMut::new(&mut buf)];
        let mut cmsg = nix::cmsg_space!([RawFd; 8]);
        let msg: nix::sys::socket::RecvMsg<()> =
            recvmsg(self.fd.as_raw_fd(), &mut iov, Some(&mut cmsg), MsgFlags::empty()).context("recvmsg")?;
        let mut fds = Vec::new();
        if let Ok(cmsgs) = msg.cmsgs() {
            for c in cmsgs {
                if let ControlMessageOwned::ScmRights(raw) = c {
                    // SAFETY: SCM_RIGHTS handed us fresh owned fds.
                    fds.extend(raw.into_iter().map(|f| unsafe { OwnedFd::from_raw_fd(f) }));
                }
            }
        }
        let n = msg.bytes;
        if n == 0 {
            return Err(anyhow!("peer closed"));
        }
        let dm = serde_json::from_slice(&buf[..n]).context("decode DaemonMsg")?;
        Ok((dm, fds))
    }

    /// Send a `ServerMsg` (no fds).
    pub fn send(&self, msg: &ServerMsg) -> Result<()> {
        let json = serde_json::to_vec(msg)?;
        let iov = [IoSlice::new(&json)];
        let cmsgs: &[ControlMessage] = &[];
        sendmsg::<()>(self.fd.as_raw_fd(), &iov, cmsgs, MsgFlags::empty(), None).context("sendmsg")?;
        Ok(())
    }
}

fn recv_packet_len(fd: RawFd) -> Result<usize> {
    let mut one = [0u8; 1];
    let mut iov = [IoSliceMut::new(&mut one)];
    let msg: nix::sys::socket::RecvMsg<()> =
        recvmsg(fd, &mut iov, None, MsgFlags::MSG_PEEK | MsgFlags::MSG_TRUNC)
            .context("recvmsg peek")?;
    let n = msg.bytes;
    if n == 0 {
        return Err(anyhow!("peer closed"));
    }
    if n > MAX_PACKET_BYTES {
        return Err(anyhow!("packet too large: {n} bytes > {MAX_PACKET_BYTES}"));
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::socket::connect;
    use wire::socket::{ClipboardData, DaemonMsg};

    fn tmp_sock_path(name: &str) -> String {
        std::env::temp_dir()
            .join(format!("rmng-{name}-{}-{}.sock", std::process::id(), rand_suffix()))
            .to_string_lossy()
            .into_owned()
    }

    fn rand_suffix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    fn send_daemon_msg(path: &str, msg: &DaemonMsg) -> Result<()> {
        let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::empty(), None)
            .context("socket")?;
        connect(fd.as_raw_fd(), &UnixAddr::new(path).unwrap()).context("connect")?;
        let json = serde_json::to_vec(msg)?;
        let iov = [IoSlice::new(&json)];
        sendmsg::<()>(fd.as_raw_fd(), &iov, &[], MsgFlags::empty(), None).context("sendmsg")?;
        Ok(())
    }

    #[test]
    fn recv_accepts_large_clipboard_data_messages() {
        let path = tmp_sock_path("large-clip");
        let listener = Listener::bind(&path).unwrap();
        let payload = vec![0x42; 128 * 1024];
        let sent = DaemonMsg::ClipboardData(ClipboardData {
            serial: 7,
            mime_type: "image/png".into(),
            bytes: payload.clone(),
        });

        send_daemon_msg(&path, &sent).unwrap();
        let conn = listener.accept().unwrap();
        let (got, fds) = conn.recv().unwrap();
        let _ = std::fs::remove_file(&path);

        assert!(fds.is_empty());
        assert_eq!(got, sent);
    }
}
