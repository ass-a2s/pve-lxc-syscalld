use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::io as StdIo;
use std::io::{stderr, stdout, Write};
use std::os::unix::ffi::OsStrExt;

use failure::{bail, format_err, Error};
use nix::sys::socket::SockAddr;

#[macro_use]
mod macros;

pub mod apparmor;
pub mod capability;
pub mod client;
pub mod error;
pub mod fork;
pub mod io;
pub mod lxcseccomp;
pub mod nsfd;
pub mod poll_fn;
pub mod process;
pub mod seccomp;
pub mod sys_mknod;
pub mod sys_quotactl;
pub mod syscall;
pub mod tools;

use crate::io::seq_packet::SeqPacketListener;

pub fn spawn(fut: impl Future<Output = ()> + Send + 'static) {
    tokio::spawn(fut);
}

fn usage(status: i32, program: &OsStr, out: &mut dyn Write) -> ! {
    let _ = out.write_all("usage: ".as_bytes());
    let _ = out.write_all(program.as_bytes());
    let _ = out.write_all(
        concat!(
            "[options] SOCKET_PATH\n",
            "options:\n",
            "    -h, --help      show this help message\n",
            "    --system        \
                     run as systemd daemon (use sd_notify() when ready to accept connections)\n",
        )
        .as_bytes(),
    );
    std::process::exit(status);
}

fn main() {
    let mut args = std::env::args_os();
    let program = args.next().unwrap(); // program name always exists

    let mut use_sd_notify = false;
    let mut path = None;

    for arg in &mut args {
        if arg == "-h" || arg == "--help" {
            usage(0, &program, &mut stdout());
        }

        if arg == "--" {
            break;
        } else if arg == "--system" {
            use_sd_notify = true;
        } else {
            let bytes = arg.as_bytes();
            if bytes.starts_with(b"-") {
                let _ = stderr().write_all(b"unexpected option: ");
                let _ = stderr().write_all(arg.as_bytes());
                usage(1, &program, &mut stderr());
            }

            if path.is_some() {
                let _ = stderr().write_all(b"unexpected extra parameter: ");
                let _ = stderr().write_all(arg.as_bytes());
                usage(1, &program, &mut stderr());
            }

            path = Some(arg);
        }
    }

    let path = match path {
        Some(path) => path,
        None => {
            eprintln!("missing path");
            usage(1, &program, &mut stderr());
        }
    };

    let mut rt = tokio::runtime::Runtime::new().expect("failed to spawn tokio runtime");

    if let Err(err) = rt.block_on(do_main(use_sd_notify, path)) {
        eprintln!("error: {}", err);
        std::process::exit(1);
    }
}

async fn do_main(use_sd_notify: bool, socket_path: OsString) -> Result<(), Error> {
    match std::fs::remove_file(&socket_path) {
        Ok(_) => (),
        Err(ref e) if e.kind() == StdIo::ErrorKind::NotFound => (), // Ok
        Err(e) => bail!("failed to remove previous socket: {}", e),
    }

    let address =
        SockAddr::new_unix(socket_path.as_os_str()).expect("cannot create struct sockaddr_un?");

    let mut listener = SeqPacketListener::bind(&address)
        .map_err(|e| format_err!("failed to create listening socket: {}", e))?;

    if use_sd_notify {
        notify_systemd()?;
    }

    loop {
        let client = listener.accept().await?;
        let client = client::Client::new(client);
        spawn(client.main());
    }
}

#[link(name = "systemd")]
extern "C" {
    fn sd_notify(unset_environment: libc::c_int, state: *const libc::c_char) -> libc::c_int;
}

fn notify_systemd() -> StdIo::Result<()> {
    let err = unsafe { sd_notify(0, c_str!("READY=1\n").as_ptr()) };
    if err >= 0 {
        Ok(())
    } else {
        Err(StdIo::Error::from_raw_os_error(-err))
    }
}
