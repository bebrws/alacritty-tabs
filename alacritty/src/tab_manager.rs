// #[cfg(not(target_os = "windows"))]
// use libc::winsize;


use std::{
    thread,
};

use anyhow::Result;
use std::sync::{Arc, RwLock};
use std::time::Instant;
use std::panic;

pub const DEFAULT_SHELL: &str = "/bin/zsh";

use log::{error, info};

use pad::PadStr;

use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;

use std::io::Read;

use alacritty_terminal::term::SizeInfo;

use crate::child_pty::Pty;

use alacritty_terminal::event::EventListener;

use crate::config::Config;


const TAB_TITLE_WIDTH: usize = 8;

pub struct TabManager<T> {
    pub current_working_directory_update: Arc<RwLock<Option<String>>>,
    pub to_exit: RwLock<bool>,
    pub selected_tab: RwLock<Option<usize>>,
    pub tabs: RwLock<Vec<Tab<T>>>,
    pub size: RwLock<Option<SizeInfo>>,
    pub event_proxy: T,
    pub config: Config,
    pub last_update: std::time::Instant,
    pub tab_titles: RwLock<Vec<String>>,
}

impl<T: Clone + EventListener + Send + 'static> TabManager<T> {
    pub fn new(event_proxy: T, config: Config) -> TabManager<T> {
        Self {
            current_working_directory_update: Arc::new(RwLock::new(None)),
            to_exit: RwLock::new(false),
            selected_tab: RwLock::new(None),
            tabs: RwLock::new(Vec::new()),
            size: RwLock::new(None),
            event_proxy,
            config,
            last_update: Instant::now(),
            tab_titles: RwLock::new(Vec::new()),
        }
    }

    pub fn resize(&self, sz: SizeInfo) {
        loop {
            if let Ok(mut size_write_guard) = self.size.try_write() {
                *size_write_guard = Some(sz);
                break;
            }
        }
        loop {
            if let Ok(tabs_read_guard) = self.tabs.try_read() {
                for tab in (*tabs_read_guard).iter() {
                    let terminal_mutex = tab.terminal.clone();
                    let mut terminal_guard = terminal_mutex.lock();
                    let terminal = &mut *terminal_guard;
                    let term_sz = sz;
                    terminal.resize(term_sz);
                    drop(terminal_guard);

                    let pty_mutex = tab.pty.clone();
                    let mut pty_guard = pty_mutex.lock();
                    let pty = &mut *pty_guard;
                    let pty_sz = sz;
                    pty.on_resize(&pty_sz);
                    drop(pty_guard);
                }
                break;
            }
        }
    }

    pub fn set_size(&self, sz: SizeInfo) {
        loop {
            if let Ok(mut size_write_guard) = self.size.try_write() {
                *size_write_guard = Some(sz);
                break;
            }
        }
    }

    #[inline]
    pub fn num_tabs(&self) -> usize {
        loop {
            if let Ok(tabs_read_guard) = self.tabs.try_read() {
                return (*tabs_read_guard).len();
            }
        }
    }

    pub fn new_tab(&self) -> Result<usize> {
        let current_working_directory_option: Option<String>;

        loop {
            if let Ok(current_working_directory_update_guard) = self.current_working_directory_update.try_read() {
                let current_working_directory_update = &*current_working_directory_update_guard;
                let current_working_directory_update_clone = current_working_directory_update.clone();
                current_working_directory_option = if current_working_directory_update.is_some() {
                    Some(current_working_directory_update_clone.unwrap().clone())
                } else {
                    None
                };
                break;
            }
        }
        
        let tab_idx = match self.selected_tab_idx() {
            Some(idx) => idx + 1,
            None => 0,
        };

        info!("Creating new tab {}\n", tab_idx);
        info!("Default shell {}\n", DEFAULT_SHELL);

        let size_info_option: Option<SizeInfo>;
        loop {
            if let Ok(size_read_guard) = self.size.try_read() {
                size_info_option = *size_read_guard;
                break;
            }
        }
        if size_info_option.is_none() {
            panic!("Unable to read the current terminal size");
        };

        let sz = size_info_option.unwrap();
        let new_tab = Tab::new(
            sz,
            self.config.clone(),
            self.event_proxy.clone(),
            current_working_directory_option,
            self.current_working_directory_update.clone()
        );

        let pty_arc = new_tab.pty.clone();
        let mut pty_guard = pty_arc.lock();
        let unlocked_pty = &mut *pty_guard;
        let mut pty_output_file = unlocked_pty.fin_clone();
        drop(pty_guard);

        let terminal_arc = new_tab.terminal.clone();

        let mut sel_tab_idx: usize = 0;
        if let Ok(sel_tab_idx_guard) = self.selected_tab.read() {
            let sel_tab_idx_option = & *sel_tab_idx_guard;
            sel_tab_idx = match sel_tab_idx_option {
                Some(tab_idx) => {
                    *tab_idx
                },
                None => {
                    // *sel_tab_idx_option = Some(0);
                    0
                }
            }
        }

        loop {
            if let Ok(mut tabs_write_guard) = self.tabs.try_write() {
                let tabs = &mut *tabs_write_guard;
                let tabs_len = tabs.len();
                
                if tabs_len > 1 {
                    let one_less = tabs_len - 1;
                    if sel_tab_idx != one_less {
                        let mut new_tabs: Vec<Tab<T>> = Vec::new();

                        new_tabs.append(&mut tabs[0..sel_tab_idx+1].to_vec());
                        new_tabs.append(&mut vec![new_tab]);
                        new_tabs.append(&mut tabs[(sel_tab_idx+1)..tabs_len].to_vec());
                        *tabs = new_tabs;
                    } else {
                        (*tabs_write_guard).push(new_tab);
                    }
                } else {
                    (*tabs_write_guard).push(new_tab);
                }
                break;
            }
        }


        if self.num_tabs() == 1 {
            self.set_selected_tab(0);
        }

        info!("Inserted and selected new tab {}\n", tab_idx);

        let event_proxy_clone = self.event_proxy.clone();
        thread::spawn( move || {
            let mut processor = alacritty_terminal::ansi::Processor::new();
            loop {
                let mut buffer: [u8; crate::child_pty::PTY_BUFFER_SIZE] =
                    [0; crate::child_pty::PTY_BUFFER_SIZE];

                match pty_output_file.read(&mut buffer) {
                    Ok(rlen) => {
                        if rlen > 0 {
                            let mut terminal_guard = terminal_arc.lock();
                            let terminal = &mut *terminal_guard;
                            let mut pty_guard = pty_arc.lock();
                            let mut unlocked_pty = &mut *pty_guard;

                            buffer.iter().for_each(|byte| {
                                processor.advance(terminal, *byte, &mut unlocked_pty)
                            });

                            drop(pty_guard);
                            drop(terminal_guard);
                        }

                        if rlen == 0 {
                            // Close this tty
                            event_proxy_clone.send_event(alacritty_terminal::event::Event::Close);
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

    pub fn set_selected_tab(&self, idx: usize) {
        loop {
            if let Ok(mut write_guard) = self.selected_tab.try_write() {
                *write_guard = Some(idx);
                break;
            }
        }
    }

    pub fn remove_selected_tab(&self) {
        if let Some(idx) = self.selected_tab_idx() {
            loop {
                if let Ok(mut rwlock_tabs) = self.tabs.try_write() {
                    rwlock_tabs.remove(idx);
                    break;
                }
            }
        }

        if self.num_tabs() == 0 {
            match self.new_tab() {
                Ok(_idx) => {
                    self.set_selected_tab(0);
                },
                Err(e) => {
                    error!("Attempted to remove a tab when no tabs exist: {}", e);
                },
            }
        } else {
            let cur_sel_tab_idx = match self.selected_tab_idx() {
                Some(idx) => idx,
                None => 0
            };

            let tabs_len = self.tabs_len();
            if cur_sel_tab_idx < tabs_len - 1 {
                // Don't move the selected tab
            } else {
                let new_tab_idx = if tabs_len > 1 { cur_sel_tab_idx - 1 } else { 0 };
                self.set_selected_tab(new_tab_idx);
            }
        }
    }

    pub fn get_selected_tab_pty(&self) -> Arc<FairMutex<Pty>> {
        self.selected_tab_arc().pty.clone()
    }

    pub fn get_selected_tab_terminal(&self) -> Arc<FairMutex<Term<T>>> {
        self.selected_tab_arc().terminal.clone()
    }

    pub fn tabs_len(&self) -> usize {
        loop {
            if let Ok(all_cur_tabs_guard) = self.tabs.try_read() {
                let all_cur_tabs = &*all_cur_tabs_guard;
                return all_cur_tabs.len();
            }
        }
    }

    pub fn update_tab_titles(&self) {
        let mut tab_idx: usize = 0;
        loop {
            if let Ok(all_cur_tabs_guard) = self.tabs.try_read() {
                let all_cur_tabs = &*all_cur_tabs_guard;

                let tabs_count = all_cur_tabs.len();
                if tabs_count == 0 {
                    loop {
                        if let Ok(mut tab_titles_guard) = self.tab_titles.try_write() {
                            let tab_titles = &mut *tab_titles_guard;
                            *tab_titles = Vec::new();
                            break;
                        }
                    }
                } else {
                    let selected_tab_idx = match self.selected_tab_idx() {
                        Some(idx) => idx,
                        None => 0
                    };

                    loop {
                        if let Ok(mut tab_titles_guard) = self.tab_titles.try_write() {
                            let tab_titles = &mut *tab_titles_guard;
                            *tab_titles = all_cur_tabs.iter().map(|cur_tab| {
                                let term_guard = cur_tab.terminal.lock();
                                let term = &*term_guard;
                                let formatted_title: String;

                                let selected_tab_char = if tab_idx == selected_tab_idx { "*".to_string() } else { "".to_string() };

                                if  let Some(actual_title_string) = &term.title {
                                    if actual_title_string.len() > TAB_TITLE_WIDTH {
                                        formatted_title = actual_title_string[(actual_title_string.len() - 8)..].to_string()
                                    } else {
                                        formatted_title = actual_title_string.with_exact_width(TAB_TITLE_WIDTH);
                                    }
        
                                } else {
                                    // let temp_formatted_title = format!("[*{:0>8}]", tab_idx);
                                    let temp_formatted_title = format!("{}", tab_idx);
                                    formatted_title = temp_formatted_title.pad(TAB_TITLE_WIDTH, ' ', pad::Alignment::Left, true)
                                }

                                let final_formatted_title = format!("{}{}", selected_tab_char, formatted_title);
        
                                tab_idx += 1;
                                final_formatted_title
                            }).collect();
                            break;
                        }
                    }
                }
                break;
            }
        }
    }

    fn selected_tab_arc(&self) -> Arc<Tab<T>> {
        match self.selected_tab_idx() {
            Some(sel_idx) => {
                loop {
                    if let Ok(tabs_guard) = self.tabs.try_read() {
                        let tabs = & *tabs_guard;
                        let tab = tabs.get(sel_idx).unwrap();
                        let tab_clone = tab.clone();
                        return Arc::new(tab_clone);
                    }
                }
            },
            None => {
                if self.num_tabs() == 0 {
                    match self.new_tab() {
                        Ok(idx) => {
                            info!("Created new tab {}", idx);
                        },
                        Err(e) => {
                            error!("Error creating new tab: {}", e);
                        },
                    }
                }
                self.set_selected_tab(0);
                loop {
                    if let Ok(tabs_guard) = self.tabs.try_read() {
                        let tabs = & *tabs_guard;
                        let tab = tabs.get(0).unwrap().clone();
                        return Arc::new(tab);
                    }
                }
            },
        }
    }

    pub fn select_tab(& self, idx: usize) -> Option<usize> {
        self.set_selected_tab(idx);
        self.selected_tab_idx()
    }

    #[inline]
    pub fn selected_tab_idx(&self) -> Option<usize> {
        loop {
            if let Ok(selected_tab_guard) = self.selected_tab.try_read() {
                return *selected_tab_guard;
            }
        }
    }

    /// Get index of next oldest tab.
    pub fn next_tab_idx(&self) -> Option<usize> {
        match self.selected_tab_idx() {
            Some(idx) => {
                if self.num_tabs() == 0 {
                    None
                } else if idx + 1 >= self.num_tabs() {
                    Some(0)
                } else {
                    Some(idx + 1)
                }
            },
            None => None,
        }
    }

    /// Get index of next older tab.
    pub fn prev_tab_idx(&self) -> Option<usize> {
        match self.selected_tab_idx() {
            Some(idx) => {
                if idx == 0 {
                    if self.num_tabs() > 1 {
                        Some(self.num_tabs() - 1)
                    } else {
                        Some(0)
                    }
                } else {
                    Some(idx - 1)
                }
            },
            None => None,
        }
    }
}

#[derive(Clone)]
pub struct Tab<T> {
    pub pty: Arc<FairMutex<Pty>>,
    pub terminal: Arc<FairMutex<Term<T>>>,
}

impl<T: Clone + EventListener> Tab<T> {
    pub fn new(
        size: SizeInfo,
        config: Config,
        event_proxy: T,
        current_working_directory: Option<String>,
        current_working_directory_update: Arc<RwLock<Option<String>>>
    ) -> Tab<T> {
        let terminal = Term::new(&config, size, event_proxy, current_working_directory_update);
        let terminal = Arc::new(FairMutex::new(terminal));

        let pty = Arc::new(FairMutex::new(crate::child_pty::new(config, size, current_working_directory).unwrap()));

        Tab { pty, terminal }
    }
}
