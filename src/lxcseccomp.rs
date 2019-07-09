//! Module for LXC specific related seccomp handling.

use std::convert::TryFrom;
use std::mem;

use failure::{bail, Error};
use lazy_static::lazy_static;
use libc::pid_t;

use crate::seccomp::{SeccompNotif, SeccompNotifResp, SeccompNotifSizes};
use crate::socket::AsyncSeqPacketSocket;
use crate::tools::{Fd, IoVec, IoVecMut};

/// Seccomp notification proxy message sent by the lxc monitor.
///
/// Whenever a process in a container triggers a seccomp notification, and lxc has a seccomp
/// notification proxy configured, this is sent over to the proxy, together with a `SeccompNotif`,
/// `SeccompNotifResp` and a cookie.
///
/// Using this struct may be inconvenient. See the [`ProxyMessageBuffer`] for a convenient helper
/// for communcation.
#[repr(C)]
pub struct SeccompNotifyProxyMsg {
    /// Reserved data must be zero.
    reserved0: u64,

    /// The lxc monitor pid.
    ///
    /// Unless some other proxy forwards proxy messages, this should be the same pid as the peer
    /// we receive this message from.
    monitor_pid: pid_t,

    /// The container's init pid.
    ///
    /// If supported by the kernel, the lxc monitor should keep a pidfd open to this process, so
    /// this pid should be valid as long as `monitor_pid` is valid.
    init_pid: pid_t,

    /// Information about the seccomp structure sizes.
    ///
    /// This must be equal to `SeccompNotifSizes::get()`, otherwise the proxy and lxc monitor have
    /// inconsistent views of the kernel's seccomp API.
    sizes: SeccompNotifSizes,

    /// The length of the container's configured `lxc.seccomp.notify.cookie` value.
    cookie_len: u64,
}

/// Helper to receive and verify proxy notification messages.
pub struct ProxyMessageBuffer {
    proxy_msg: SeccompNotifyProxyMsg,
    seccomp_notif: SeccompNotif,
    seccomp_resp: SeccompNotifResp,
    cookie_buf: Vec<u8>,

    sizes: SeccompNotifSizes,
    seccomp_packet_size: usize,

    pid_fd: Option<Fd>,
    mem_fd: Option<Fd>,
}

unsafe fn io_vec_mut<T>(value: &mut T) -> IoVecMut {
    IoVecMut::new(std::slice::from_raw_parts_mut(
        value as *mut T as *mut u8,
        mem::size_of::<T>(),
    ))
}

unsafe fn io_vec<T>(value: &T) -> IoVec {
    IoVec::new(std::slice::from_raw_parts(
        value as *const T as *const u8,
        mem::size_of::<T>(),
    ))
}

lazy_static! {
    static ref SECCOMP_SIZES: SeccompNotifSizes = SeccompNotifSizes::get_checked()
        .map_err(|e| panic!("{}\nrefusing to run", e))
        .unwrap();
}

impl ProxyMessageBuffer {
    /// Allocate a new proxy message buffer with a specific maximum cookie size.
    pub fn new(max_cookie: usize) -> Self {
        let sizes = SECCOMP_SIZES.clone();

        let seccomp_packet_size = mem::size_of::<SeccompNotifyProxyMsg>()
            + sizes.notif as usize
            + sizes.notif_resp as usize;

        Self {
            proxy_msg: unsafe { mem::zeroed() },
            seccomp_notif: unsafe { mem::zeroed() },
            seccomp_resp: unsafe { mem::zeroed() },
            cookie_buf: unsafe { super::tools::vec::uninitialized(max_cookie) },
            sizes,
            seccomp_packet_size,
            pid_fd: None,
            mem_fd: None,
        }
    }

    /// Returns None on EOF.
    pub async fn recv(
        &mut self,
        socket: &AsyncSeqPacketSocket,
    ) -> Result<bool, Error> {
        self.proxy_msg.cookie_len = 0;

        unsafe {
            self.cookie_buf.set_len(self.cookie_buf.capacity());
        }

        let mut iovec = [
            unsafe { io_vec_mut(&mut self.proxy_msg) },
            unsafe { io_vec_mut(&mut self.seccomp_notif) },
            unsafe { io_vec_mut(&mut self.seccomp_resp) },
            IoVecMut::new(self.cookie_buf.as_mut_slice()),
        ];

        unsafe {
            self.cookie_buf.set_len(0);
        }

        let (size, fds) = socket.recv_fds_vectored(&mut iovec, 2).await?;
        if size == 0 {
            return Ok(false);
        }

        self.set_len(size)?;

        let mut fds = fds.into_iter();
        self.pid_fd = fds.next();
        self.mem_fd = fds.next();
        if self.mem_fd.is_none() {
            self.drop_fds();
            bail!("missing file descriptors with proxied seccomp message");
        }

        Ok(true)
    }

    pub fn drop_fds(&mut self) {
        self.pid_fd = None;
        self.mem_fd = None;
    }

    /// Send the current data as response.
    pub async fn respond(&mut self, socket: &AsyncSeqPacketSocket) -> std::io::Result<()> {
        let iov = [
            unsafe { io_vec(&self.proxy_msg) },
            unsafe { io_vec(&self.seccomp_notif) },
            unsafe { io_vec(&self.seccomp_resp) },
        ];
        socket.sendmsg_vectored(&iov).await
    }

    #[inline]
    fn prepare_response(&mut self) {
        let id = self.request().id;
        let resp = self.response_mut();
        resp.id = id;
        resp.val = -1;
        resp.error = -libc::ENOSYS;
        resp.flags = 0;
    }

    /// Called by with_io_slice after the callback returned the new size. This verifies that
    /// there's enough data available.
    pub fn set_len(&mut self, len: usize) -> Result<(), Error> {
        if len < self.seccomp_packet_size {
            bail!("seccomp proxy message too short");
        }

        if self.proxy_msg.reserved0 != 0 {
            bail!("reserved data wasn't 0, liblxc secocmp notify protocol mismatch");
        }

        if !self.check_sizes() {
            bail!("seccomp proxy message content size validation failed");
        }

        if len - self.seccomp_packet_size > self.cookie_buf.capacity() {
            bail!("seccomp proxy message too long");
        }

        let cookie_len = match usize::try_from(self.proxy_msg.cookie_len) {
            Ok(cl) => cl,
            Err(_) => {
                self.proxy_msg.cookie_len = 0;
                bail!("cookie length exceeds our size type!");
            }
        };

        if len != self.seccomp_packet_size + cookie_len {
            bail!(
                "seccomp proxy packet contains unexpected cookie length {} + {} != {}",
                self.seccomp_packet_size,
                cookie_len,
                len
            );
        }

        unsafe {
            self.cookie_buf.set_len(cookie_len);
        }

        self.prepare_response();

        Ok(())
    }

    fn check_sizes(&self) -> bool {
        let got = self.proxy_msg.sizes.clone();
        got.notif == self.sizes.notif
            && got.notif_resp == self.sizes.notif_resp
            && got.data == self.sizes.data
    }

    /// Get the monitor pid from the current message.
    ///
    /// There's no guarantee that the pid is valid.
    pub fn monitor_pid(&self) -> pid_t {
        self.proxy_msg.monitor_pid
    }

    /// Get the container's init pid from the current message.
    ///
    /// There's no guarantee that the pid is valid.
    pub fn init_pid(&self) -> pid_t {
        self.proxy_msg.init_pid
    }

    /// Get the syscall request structure of this message.
    pub fn request(&self) -> &SeccompNotif {
        &self.seccomp_notif
    }

    /// Access the response buffer of this message.
    pub fn response_mut(&mut self) -> &mut SeccompNotifResp {
        &mut self.seccomp_resp
    }

    /// Get the cookie's length.
    pub fn cookie_len(&self) -> usize {
        usize::try_from(self.proxy_msg.cookie_len).expect("cookie size should fit in an usize")
    }

    /// Get the cookie sent along with this message.
    pub fn cookie(&self) -> &[u8] {
        &self.cookie_buf
    }
}
