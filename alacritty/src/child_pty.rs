use std::{
    ffi::OsStr,
    fs::File,
    io::Read,
    os::unix::io::{FromRawFd, RawFd},
    os::unix::process::CommandExt,
    process::{Command, Stdio},
    thread,
};
use std::os::unix::io::AsRawFd;

use alacritty_terminal::tty::ToWinsize;

use std::sync::{Arc, Mutex};
use mio::event::{Iter};
use mio::{self, Token, Poll, Events, PollOpt, Ready};
use mio::unix::UnixReady;
use mio_extras::channel::{Receiver, Sender, channel};
use alacritty_terminal::term::SizeInfo;

use std::io;

use nix::sys::termios::{self, InputFlags, SetArg};

use libc::{self, c_int, pid_t, winsize, TIOCSCTTY};

use signal_hook::{self as sighook, iterator::Signals};

// use futures::{
//     channel::mpsc::{self, Receiver},
//     executor, future,
// };

use nix::pty::openpty;

use nix::{
    unistd::setsid,
};

use log::{error, info, debug, warn};


use die::die;



pub const PTY_BUFFER_SIZE: usize = 0x500;

mod ioctl {
    nix::ioctl_none_bad!(set_controlling, libc::TIOCSCTTY);
    nix::ioctl_write_ptr_bad!(win_resize, libc::TIOCSWINSZ, libc::winsize);
}



#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PtyUpdate {
    /// The PTY has closed the file.
    Exited,
    /// PTY sends byte.
    Byte(u8),
    Bytes([u8; PTY_BUFFER_SIZE]),
    // Bytes(Vec<u8>),
}



pub struct ChildPty {
    pub fd: RawFd,
    /// The File used by this PTY.
    pub file: File,
}



impl std::io::Write for ChildPty {
    // fn write(&mut self, buf: &[u8]) -> std::io::Result<usize, TabWriteError> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // println!("WRITE: {}", String::from_utf8(buf.to_vec()).unwrap());
        self.get_file().write_all(buf)?;
        // self.get_file().flush()?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::result::Result<(), std::io::Error> 
    {
        // println!("FLUSH");
        self.get_file().flush()?;
        Ok(())
    }
}


impl Clone for ChildPty {
    fn clone(&self) -> ChildPty {
        let mut file = unsafe { File::from_raw_fd(self.fd) };

        Self {
            fd: self.fd.clone(),
            file,
        }
    }
}


impl ChildPty {

    pub fn get_file(&mut self) -> &mut File {
        &mut self.file
    }
    /// Spawn a process in a new pty.
    pub fn new<I, S>(command: &str, args: I, size: winsize) -> Result<ChildPty, ()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let pty = openpty(&size, None).unwrap();

        if let Ok(mut termios) = termios::tcgetattr(pty.master) {
            // Set character encoding to UTF-8.
            termios.input_flags.set(InputFlags::IUTF8, true);
            let _ = termios::tcsetattr(pty.master, SetArg::TCSANOW, &termios);
        }

        let mut buf = [0; 1024];
        let pw = crate::passwd::get_pw_entry(&mut buf);


        let slave = pty.slave.clone();
        let master = pty.master.clone();

        unsafe {
            Command::new(&command)
                .args(args)
                .stdin(Stdio::from_raw_fd(pty.slave))
                .stdout(Stdio::from_raw_fd(pty.slave))
                .stderr(Stdio::from_raw_fd(pty.slave))
                .env("LOGNAME", pw.name)
                .env("USER", pw.name)
                .env("HOME", pw.dir)
                .env("SHELL", crate::tab_manager::DEFAULT_SHELL)
                // .env("WINDOWID", format!("{}", window_id)) // TODO Tab id
                .pre_exec(move || {

                    // let err = setsid().map_err(crate::error::stringify);
                    let pid = setsid().map_err(|e| format!("Error occured with setsid: {}", e)).unwrap();
                    if pid.as_raw() == -1 {
                        die!("Failed to set session id: {}", io::Error::last_os_error());
                    }

                    // ioctl::set_controlling(0).unwrap(); // From session-manager

                    // From alacritty:
                    ioctl::set_controlling(slave).unwrap();

                    libc::close(slave);
                    libc::close(master);
        
                    libc::signal(libc::SIGCHLD, libc::SIG_DFL);
                    libc::signal(libc::SIGHUP, libc::SIG_DFL);
                    libc::signal(libc::SIGINT, libc::SIG_DFL);
                    libc::signal(libc::SIGQUIT, libc::SIG_DFL);
                    libc::signal(libc::SIGTERM, libc::SIG_DFL);
                    libc::signal(libc::SIGALRM, libc::SIG_DFL);

                    // TODO: IMPORTANT - Follow examples here 
                    // For using mio:
                    // https://docs.rs/signal-hook-mio/0.2.0/signal_hook_mio/v0_7/index.html
                    //
                    // let signals = Signals::new(&[sighook::SIGCHLD]).expect("error preparing signal handling");

                    // TODO Save signals and register it into some event loop
                    // Signals are registered here
                    // alacritty/alacritty_terminal/src/tty/unix.rs:274


                    Ok(())
                })
                .spawn()
                .map_err(|err| ())
                .and_then(|ch| {

                    // ch.id

                    let child = ChildPty {
                        fd: pty.master,
                        file: File::from_raw_fd(pty.master),
                    };

                    child.resize(size)?;

                    Ok(child)
                })
        }
    }

    pub fn on_resize(&mut self, size: &SizeInfo) {
        let win = size.to_winsize();

        let new_winsize = winsize {
            ws_row: win.ws_row - 1,
            ws_col: win.ws_col,
            ws_xpixel: win.ws_xpixel,
            ws_ypixel: win.ws_ypixel,
        };

        let res = unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::TIOCSWINSZ, &new_winsize as *const _) };

        if res < 0 {
            die!("ioctl TIOCSWINSZ failed: {}", io::Error::last_os_error());
        }
    }

    /// Send a resize to the process running in this PTY.
    pub fn resize(&self, size: winsize) -> Result<(), ()> {

        let new_winsize = winsize {
            ws_row: size.ws_row - 1,
            ws_col: size.ws_col,
            ws_xpixel: size.ws_xpixel,
            ws_ypixel: size.ws_ypixel,
        };

        unsafe { ioctl::win_resize(self.fd, &new_winsize) }
            .map(|_| ())
            .map_err(|_| ())
    }
}