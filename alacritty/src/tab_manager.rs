use libc::winsize;
use std::io;

use mio::unix::EventedFd;
use std::os::unix::io::AsRawFd;
use std::{
    ffi::OsStr,
    fs::File,
    io::Read,
    process::{Command, Stdio},
    thread,
};

use anyhow::Result;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use std::io::Write;

use std::ops::{Deref, Index, IndexMut, Range, RangeFrom, RangeFull, RangeInclusive, RangeTo};

use mio::event::Iter;
use mio::unix::UnixReady;
use mio::{self, Events, Poll, PollOpt, Ready, Token};
use mio_extras::channel::{channel, Receiver, Sender};

use std::marker::PhantomData;

use std::hash::{BuildHasher, Hash};

use miniserde::ser::{Fragment, Map, Seq};

use std::borrow::Cow;

use miniserde::{json, Deserialize, Serialize};

pub const DEFAULT_SHELL: &str = "/bin/zsh";

use log::{debug, error, info, warn};

use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::tty;

use alacritty_terminal::term::SizeInfo;

use crate::child_pty::ChildPty;
use crate::child_pty::PtyUpdate;

use crate::event::EventProxy;
use thiserror::Error;

use crate::config::Config;

const DELAY_DURATION: Duration = Duration::from_millis(400);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TabManagerUpdate {
    tab_idx: usize,
    data: PtyUpdate,
}

macro_rules! seltab_cl {
    ($seltab:expr, $terminal:ident,  { $($b:tt)* } ) => {
        let tab = $seltab.unwrap();
        let terminal_mutex =  tab.terminal.clone();
        let mut terminal_guard = terminal_mutex.lock();
        let mut $terminal = &mut *terminal_guard;

        $($b)*

        drop(terminal_guard);


    };
}

#[derive(Debug)]
pub enum Msg {
    /// Data that should be written to the PTY.
    Input(Vec<u8>),

    /// Instruction to resize the PTY.
    Resize(SizeInfo),
}

pub struct TabManager {
    selected_tab: Option<usize>,
    pub tabs: Vec<Tab>,
    pub size: Option<SizeInfo>,
    pub event_proxy: crate::event::EventProxy,
    pub config: Config,
    pub last_update: std::time::Instant,
}

impl TabManager {
    pub fn new(event_proxy: crate::event::EventProxy, config: Config) -> TabManager {
        
        let mut tm = Self {
            selected_tab: None,
            tabs: Vec::new(),
            size: None,
            event_proxy,
            config,
            last_update: Instant::now(),
        };

        return tm;
    }

    pub fn resize(&mut self, sz: SizeInfo) {
        self.size = Some(sz);

        let tab_r = &mut self.tabs;

        for tab in tab_r.into_iter() {
            let terminal_mutex = tab.terminal.clone();
            let mut terminal_guard = terminal_mutex.lock();
            let mut terminal = &mut *terminal_guard;
            let term_sz = sz.clone();
            terminal.resize(term_sz);
            drop(terminal_guard);
    
            let pty_mutex = tab.pty.clone();
            let mut pty_guard = pty_mutex.lock();
            let mut pty = &mut *pty_guard;
            let pty_sz = sz.clone();
            pty.on_resize(&pty_sz);
            drop(pty_guard);
        }
    }

    pub fn set_size(&mut self, size: SizeInfo) {
        self.size = Some(size.clone());
    }

    pub fn get_next_tab(&mut self) -> usize {
        match self.selected_tab {
            Some(idx) => {
                if idx + 1 >= self.tabs.len() {
                    0
                } else {
                    idx + 1
                }
            },
            None => {
                0
            }
        }
    }

    pub fn new_tab(&mut self) -> Result<usize> {
        let tab_idx = match self.selected_tab {
            Some(idx) => {
                idx + 1
            },
            None => {
                0
            }
        };
        info!("Creating new tab {}\n", tab_idx);
        info!("Default shell {}\n", DEFAULT_SHELL);
        let szinfo = self.size.unwrap();
        let new_tab = Tab::new(
            DEFAULT_SHELL,
            szinfo.clone(),
            self.config.clone(),
            self.event_proxy.clone(),
            self,
        );

        let pty_arc = new_tab.pty.clone();
        let mut pty_guard = pty_arc.lock();
        let mut unlocked_pty = &mut *pty_guard;
        let raw_fd: std::os::unix::io::RawFd = unlocked_pty.file.as_raw_fd();
        let mut pty_output_file = unlocked_pty.file.try_clone().unwrap();
        drop(pty_guard);

        let terminal_arc = new_tab.terminal.clone();
        self.tabs.push(new_tab);

        if self.tabs.len() == 1 {
            self.selected_tab = Some(0);
        }

        info!("Inserted and selected new tab {}\n", tab_idx);
        
        

        let event_proxy = self.event_proxy.clone();
        thread::spawn(move || {
            let mut processor = alacritty_terminal::ansi::Processor::new();
            loop {
                let mut buffer: [u8; crate::child_pty::PTY_BUFFER_SIZE] =
                    [0; crate::child_pty::PTY_BUFFER_SIZE];
                    
                match pty_output_file.read(&mut buffer) {
                    Ok(rlen) => {
                        if rlen > 0 {
                            let mut terminal_guard = terminal_arc.lock();
                            let mut terminal = &mut *terminal_guard;
                            let mut pty_guard = pty_arc.lock();
                            let mut unlocked_pty = &mut *pty_guard;
                            // terminal.dirty = true;
                            // event_proxy.send_event(crate::event::Event::TerminalEvent(
                            //     alacritty_terminal::event::Event::Wakeup,
                            // ));
                            buffer.into_iter().for_each(|byte| {
                                processor.advance(terminal, *byte, &mut unlocked_pty)
                            });
                            drop(pty_guard);
                            drop(terminal_guard);
                        }

                        if rlen == 0 {
                            // Close this tty
                            event_proxy.send_event(crate::event::Event::TerminalEvent(
                                    alacritty_terminal::event::Event::Close(tab_idx),
                            ));
                            break; // break out of loop
                        }
                    },
                    Err(e) => {
                        error!("Error {} reading bytes from tty", e);
                    },
                }
            }
        });

        Ok(tab_idx)
    }

    pub fn remove_selected_tab(&mut self) {
        match self.selected_tab {
            Some(idx) => {
                self.tabs.remove(idx);
            },
            None => {

            }
        };

        if self.tabs.len() == 0 {
            match self.new_tab() {
                Ok(idx) => {
                    // println!("Creating new tab as removed last tab, selected is 0");
                    self.selected_tab = Some(0);
                },
                Err(e) => {
                    // println!("Error creating new tab");
                }
            }
        } else {
            let next_idx = self.next_tab_idx().unwrap();
            if next_idx >= self.tabs.len() {
                // println!("Invalid next selected tab is {}", self.tabs.len() - 1);
                self.selected_tab = Some(self.tabs.len() - 1);
            } else {
                // println!("Next selected tab is {}", next_idx);
                self.selected_tab = Some(next_idx);
            }
        }
    }

    pub fn selected_tab(&mut self) -> Option<&Tab> {
        match self.selected_tab {
            Some(sel_idx) => {
                self.tabs.get(sel_idx)
            },
            None => {
                if self.tabs.len() == 0 {
                    match self.new_tab() {
                        Ok(idx) => {
                            // println!("Created new tab {}", idx);
                        },
                        Err(e) => {
                            error!("Error creating new tab");
                        }
                    }
                }
                self.selected_tab = Some(0);
                self.tabs.get(0)
            }
        }
    }

    pub fn selected_tab_mut(&mut self) -> Option<&mut Tab> {
        match self.selected_tab {
            Some(sel_idx) => {
                self.tabs.get_mut(sel_idx)
            },
            None => {
                if self.tabs.len() == 0 {
                    match self.new_tab() {
                        Ok(idx) => {
                            // println!("Created new tab {}", idx);
                        },
                        Err(e) => {
                            error!("Error creating new tab");
                        }
                    }
                }
                self.selected_tab = Some(0);
                self.tabs.get_mut(0)
            }
        }
    }

    pub fn select_tab(&mut self, idx: usize) -> Option<usize> {
        match self.tabs.get(idx) {
            Some(current_tab) => {
                let new_sz = self.size.clone();

                self.selected_tab = Some(idx);

                let tab: &mut Tab = self.selected_tab_mut().expect("existed during match");

                // tab.resize(new_sz);
                // tab.mark_dirty();
                Some(idx)
            },
            None => None,
        }
    }

    pub fn selected_tab_idx(&self) -> Option<usize> {
        self.selected_tab
    }

    /// Get index of next oldest tab.
    pub fn next_tab_idx(&self) -> Option<usize> {
        match self.selected_tab {
            Some(idx) => {
                if self.tabs.len() == 0 {
                    None
                } else if idx + 1 >= self.tabs.len() {
                    Some(0)
                } else {
                    Some(idx + 1)
                }
            },
            None => {
                None
            }
        }
    }

    /// Get index of next older tab.
    pub fn prev_tab_idx(&self) -> Option<usize> {
        match self.selected_tab {
            Some(idx) => {
                if idx == 0 {
                    if self.tabs.len() > 1 {
                        Some(self.tabs.len() - 1)
                    } else {
                        Some(0)
                    }
                } else {
                    Some(idx - 1)
                }
            },
            None => {
                None
            }
        }
    }

    /// Get index of youngest tab.
    pub fn first_tab_idx(&self) -> Option<usize> {
        // Next here will just iterate to the first value
        Some(0)
    }

    /// Get index of oldest tab.
    pub fn last_tab_idx(&self) -> Option<usize> {
        // Next back will iterate back around to the last value
        Some(self.tabs.len())
    }

    /// Receive stdin for the active `Window`.
    pub fn receive_stdin(&mut self, data: &[u8]) -> Result<(), TabError> {
        Ok(self.selected_tab_mut().ok_or(TabError::NoSelectedTab)?.receive_stdin(data)?)
    }
}

pub fn stringify(err: &dyn std::fmt::Display) -> String {
    format!("error code: {}", err.to_string())
}
// pub fn stringify(err: &dyn std::error::Error) -> String { format!("error code: {}",
// err.to_string()) }

#[derive(Error, Debug)]
pub enum TabError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("no selected tab")]
    NoSelectedTab,
    #[error("attempted to select an invalid tab")]
    TabLost,
}

#[derive(Error, Debug)]
pub enum TabWriteError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("Unable to write for some other reason")]
    UnableToWriteOtherReason,
}

pub struct Tab {
    pub pty: Arc<FairMutex<ChildPty>>,
    pub terminal: Arc<FairMutex<Term<EventProxy>>>,
}

impl Tab {
    pub fn new(
        command: &str,
        size: SizeInfo,
        config: Config,
        event_proxy: crate::event::EventProxy,
        tab_manager: &mut TabManager,
    ) -> Tab {
        let terminal = Term::new(&config, size, event_proxy.clone());
        let terminal = Arc::new(FairMutex::new(terminal));

        let new_winsize = winsize {
            ws_row: size.screen_lines().0 as u16,
            ws_col: size.cols().0 as u16,
            ws_xpixel: size.width() as libc::c_ushort,
            ws_ypixel: size.height() as libc::c_ushort,
        };

        let args: [&str; 0] = [];
        let pty = Arc::new(FairMutex::new(
            ChildPty::new(command, &args, new_winsize).unwrap(),
        ));

        Tab { pty, terminal }
    }

    pub fn receive_stdin(&mut self, data: &[u8]) -> Result<(), io::Error> {
        let tab_terminal = self.terminal.clone();
        let mut terminal_guard = tab_terminal.lock();
        let terminal = &mut *terminal_guard;
        terminal.dirty = true;
        drop(terminal_guard);

        let mut pty_guard = self.pty.lock();
        let mut unlocked_pty = &mut *pty_guard;
        unlocked_pty.write(data)?;
        drop(pty_guard);
        Ok(())
    }
}
