use std::os::unix::io::AsRawFd;
use std::{
    ffi::OsStr,
    fs::File,
    io::Read,
    process::{Command, Stdio},
    thread,
};

use mio::unix::EventedFd;

use std::time::{Instant, Duration};

// use futures::{
//     channel::mpsc::{self, Receiver, Sender},
//     executor, future,
// };

use libc::winsize;

use nix::{
    unistd::setsid,
};

use mio::event::{Iter};
use mio::{self, Token, Poll, Events, PollOpt, Ready};
use mio::unix::UnixReady;
use mio_extras::channel::{Receiver, Sender, channel};


const SEND_DELAY_ERROR_MS: u64 = 200;
const PTY_CHANNEL_BUFFER_SIZE: usize = 0x4000;

use crate::child_pty::PTY_BUFFER_SIZE;
use crate::child_pty::PtyUpdate;
use crate::child_pty::ChildPty;

use log::{error, info, debug, warn, trace};


// pub fn spawn_pty<I, S>(
//     command: &str,
//     args: I,
//     size: winsize,
//     poll: mio::Poll,
//     token: Token
// ) -> ChildPty
// where
//     I: IntoIterator<Item = S>,
//     S: AsRef<OsStr>,
// {
//     let child_pty = ChildPty::new(command, args, size).unwrap();
//     //  {
//     //     Ok(r) => {
//     //         r
//     //     }, 
//     //     Err(e) => {
//     //         error!("Error poll {}", e);
//     //         ()
//     //     }
//     // };

//     let raw_fd: std::os::unix::io::RawFd = child_pty.file.as_raw_fd();


//     match poll.register(&EventedFd(&raw_fd), token, mio::Ready::writable(), PollOpt::edge() | PollOpt::oneshot()) {
//         Ok(r) => {
//             info!("Success poll");
//         }, 
//         Err(e) => {
//             error!("Error poll {}", e);
//         }
//     };

//     return child_pty;
// }