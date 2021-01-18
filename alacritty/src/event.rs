//! Process window events.

use std::borrow::Cow;
use std::cmp::{max, min};
use std::env;
use std::fmt::Debug;
#[cfg(not(any(target_os = "macos", windows)))]
use std::fs;
use std::fs::File;
use std::io::Write;
use std::mem;
use std::ops::RangeInclusive;
use std::path::PathBuf;
#[cfg(not(any(target_os = "macos", windows)))]
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use glutin::dpi::PhysicalSize;
use glutin::event::{ElementState, Event as GlutinEvent, ModifiersState, MouseButton, WindowEvent};
use glutin::event_loop::{ControlFlow, EventLoop, EventLoopProxy, EventLoopWindowTarget};
use glutin::platform::desktop::EventLoopExtDesktop;
#[cfg(all(feature = "wayland", not(any(target_os = "macos", windows))))]
use glutin::platform::unix::EventLoopWindowTargetExtUnix;
use log::info;
use serde_json as json;

#[cfg(target_os = "macos")]
use crossfont::set_font_smoothing;
use crossfont::{self, Size};

use alacritty_terminal::config::LOG_TARGET_CONFIG;
use alacritty_terminal::event::{Event as TerminalEvent, EventListener, Notify, OnResize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Boundary, Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{ClipboardType, SizeInfo, Term, TermMode};
#[cfg(not(windows))]
use alacritty_terminal::tty;

use crate::tab_manager::TabManager;

use crate::cli::Options as CLIOptions;
use crate::clipboard::Clipboard;
use crate::config;
use crate::config::Config;
use crate::daemon::start_daemon;
use crate::display::{Display, DisplayUpdate};
use crate::input::{self, ActionContext as _, FONT_SIZE_STEP};
#[cfg(target_os = "macos")]
use crate::macos;
use crate::message_bar::{Message, MessageBuffer};
use crate::scheduler::{Scheduler, TimerId};
use crate::url::{Url, Urls};
use crate::window::Window;
use alacritty_terminal::term::RenderableCellContent;

#[macro_use]
use crate::macros;

/// Duration after the last user input until an unlimited search is performed.
pub const TYPING_SEARCH_DELAY: Duration = Duration::from_millis(500);

/// Maximum number of lines for the blocking search while still typing the search regex.
const MAX_SEARCH_WHILE_TYPING: Option<usize> = Some(10000);

/// Events dispatched through the UI event loop.
#[derive(Debug, Clone)]
pub enum Event {
    TerminalEvent(TerminalEvent),
    DPRChanged(f64, (u32, u32)),
    Scroll(Scroll),
    ConfigReload(PathBuf),
    Message(Message),
    BlinkCursor,
    SearchNext,
    TestEvent,
}

impl From<Event> for GlutinEvent<'_, Event> {
    fn from(event: Event) -> Self {
        GlutinEvent::UserEvent(event)
    }
}

impl From<TerminalEvent> for Event {
    fn from(event: TerminalEvent) -> Self {
        Event::TerminalEvent(event)
    }
}

/// Regex search state.
pub struct SearchState {
    /// Search string regex.
    regex: Option<String>,

    /// Search direction.
    direction: Direction,

    /// Change in display offset since the beginning of the search.
    display_offset_delta: isize,

    /// Search origin in viewport coordinates relative to original display offset.
    origin: Point,

    /// Focused match during active search.
    focused_match: Option<RangeInclusive<Point<usize>>>,
}

impl SearchState {
    fn new() -> Self {
        Self::default()
    }

    /// Search regex text if a search is active.
    pub fn regex(&self) -> Option<&String> {
        self.regex.as_ref()
    }

    /// Direction of the search from the search origin.
    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// Focused match during vi-less search.
    pub fn focused_match(&self) -> Option<&RangeInclusive<Point<usize>>> {
        self.focused_match.as_ref()
    }
}

impl Default for SearchState {
    fn default() -> Self {
        Self {
            direction: Direction::Right,
            display_offset_delta: 0,
            origin: Point::default(),
            focused_match: None,
            regex: None,
        }
    }
}

pub struct ActionContext<'a> {
    pub tab_manager: Arc<FairMutex<TabManager>>,
    // pub terminal: &'a mut Term<EventProxy>,
    pub clipboard: &'a mut Clipboard,
    pub size_info: &'a mut SizeInfo,
    pub mouse: &'a mut Mouse,
    pub received_count: &'a mut usize,
    pub suppress_chars: &'a mut bool,
    pub modifiers: &'a mut ModifiersState,
    pub window: &'a mut Window,
    pub message_buffer: &'a mut MessageBuffer,
    pub display_update_pending: &'a mut DisplayUpdate,
    pub config: &'a mut Config,
    pub event_loop: &'a EventLoopWindowTarget<Event>,
    pub urls: &'a Urls,
    pub scheduler: &'a mut Scheduler,
    pub search_state: &'a mut SearchState,
    cursor_hidden: &'a mut bool,
    cli_options: &'a CLIOptions,
    font_size: &'a mut Size,
}

impl<'a> ActionContext<'a> {
    pub fn with_terminal<Y: FnMut(&mut Term<EventProxy>)>(&mut self, mut terminal_closure: Y) {
        tm_cl!(self.tab_manager, terminal, {
            terminal_closure(terminal);
        });
    }
}

impl<'a> input::ActionContext<EventProxy> for ActionContext<'a> {
    fn tab_manager(&self) -> Arc<FairMutex<TabManager>> {
        return self.tab_manager.clone();
    }

    fn write_to_pty<B: Into<Cow<'static, [u8]>>>(&mut self, val: B) {

        let mut tab_manager_guard = self.tab_manager.lock();
        let mut tab_manager: &mut TabManager = &mut *tab_manager_guard;
        let c: Cow<[u8]> = val.into();
        let vc: Vec<u8> = c.into_iter().map(|c| *c).collect();
        tab_manager.receive_stdin(&vc);
        drop(tab_manager_guard);
    }

    fn size_info(&self) -> SizeInfo {
        *self.size_info
    }

    fn scroll(&mut self, scroll: Scroll) {
        let mut old_offset = 0;

        tm_cl!(self.tab_manager, terminal, {
            old_offset = terminal.grid().display_offset() as isize;
            terminal.scroll_display(scroll);
        });

        // Keep track of manual display offset changes during search.
        if self.search_active() {
            tm_cl!(self.tab_manager, terminal, {
                let display_offset = terminal.grid().display_offset();
                self.search_state.display_offset_delta += old_offset - display_offset as isize;
            });
        }

        // Update selection.
        tm_cl!(self.tab_manager, terminal, {
            if terminal.mode().contains(TermMode::VI)
                && terminal.selection.as_ref().map(|s| s.is_empty()) != Some(true)
            {
                self.update_selection_wt(terminal.vi_mode_cursor.point, Side::Right, &mut terminal);
            } else if self.mouse().left_button_state == ElementState::Pressed
                || self.mouse().right_button_state == ElementState::Pressed
            {
                let point = self.size_info().pixels_to_coords(self.mouse().x, self.mouse().y);
                let cell_side = self.mouse().cell_side;
                self.update_selection_wt(
                    Point { line: point.line, col: point.col },
                    cell_side,
                    &mut terminal,
                );
            }
        });
    }

    fn copy_selection(&mut self, ty: ClipboardType) {
        tm_cl!(self.tab_manager, terminal, {
            if let Some(selected) = terminal.selection_to_string() {
                if !selected.is_empty() {
                    self.clipboard.store(ty, selected);
                }
            }
        });
    }

    fn selection_is_empty(&self) -> bool {
        tm_cl!(self.tab_manager, terminal, {
            let r = self.selection_is_empty_wt(&mut terminal);
        });

        r
    }

    fn clear_selection(&mut self) {
        tm_cl!(self.tab_manager, terminal, {
            self.clear_selection_wt(&mut terminal);
        });
    }

    fn selection_is_empty_wt(&self, terminal: &mut Term<EventProxy>) -> bool {
        terminal.selection.as_ref().map(Selection::is_empty).unwrap_or(true)
    }

    fn clear_selection_wt(&mut self, terminal: &mut Term<EventProxy>) {
        terminal.selection = None;
        terminal.dirty = true;
    }

    fn update_selection_wt(
        &mut self,
        mut point: Point,
        side: Side,
        terminal: &mut Term<EventProxy>,
    ) {
        let mut selection = match terminal.selection.take() {
            Some(selection) => selection,
            None => return,
        };

        // Treat motion over message bar like motion over the last line.
        point.line = min(point.line, terminal.screen_lines() - 1);

        // Update selection.
        let absolute_point = terminal.visible_to_buffer(point);
        selection.update(absolute_point, side);

        // Move vi cursor and expand selection.
        if terminal.mode().contains(TermMode::VI) && !self.search_active() {
            terminal.vi_mode_cursor.point = point;
            selection.include_all();
        }

        terminal.selection = Some(selection);
        terminal.dirty = true;
    }

    fn update_selection(&mut self, mut point: Point, side: Side) {
        tm_cl!(self.tab_manager, terminal, {
            self.update_selection_wt(point, side, &mut terminal);
        });
    }

    fn start_selection_wt(
        &mut self,
        ty: SelectionType,
        point: Point,
        side: Side,
        terminal: &mut Term<EventProxy>,
    ) {
        // let point_p: Point<Line> = Point::new(point.line - 1, point.col);
        terminal.selection = Some(Selection::new(ty, terminal.visible_to_buffer(point), side));
        terminal.dirty = true;
    }

    fn start_selection(&mut self, ty: SelectionType, point: Point, side: Side) {
        tm_cl!(self.tab_manager, terminal, {
            self.start_selection_wt(ty, point, side, &mut terminal);
        });
    }

    fn find_word(&mut self, point_p: Point, side: Side) -> String {
        let point:Point = Point::new(point_p.line - 1, point_p.col);
        
        let mut ret: String = "".to_string();
        tm_cl!(self.tab_manager, terminal, {
            let grid_cells =
                terminal.renderable_cells(self.config, *self.cursor_hidden).collect::<Vec<_>>();

            // let mut lastEmpty = true;
            let mut found_point = false;
            let mut found_end = false;
            let mut string = String::new();
            let mut last_column = 0;
            let mut cell_iter = grid_cells.iter();
            let mut cell_or_none = cell_iter.next();
            while found_end == false && !cell_or_none.is_none() {
                let cell = (*cell_or_none.unwrap()).clone();
                let mut c = match cell.inner.clone() {
                    RenderableCellContent::Chars(ctuple) => ctuple.0,
                    _ => ' ',
                };

                if last_column + 1 != cell.column.0 || c == ' ' {
                    if found_point {
                        found_end = true;
                    } else {
                        string = String::new();
                        if c != ' ' {
                            string.push(c.clone());
                        }
                    }
                } else {
                    string.push(c.clone());
                }

                if cell.column == point.col && cell.line == point.line {
                    found_point = true;
                }
                cell_or_none = cell_iter.next();
                last_column = cell.column.0;
            }

            if found_point {
                ret = string;
            } else {
                ret = String::new();
            }
        });

        return ret;
    }


    fn find_tab(& self, col: Column) -> Option<usize> {
        //"[000] [000] [000] "
        let mut ret: Option<usize> = None;

        let c = col.0 ;

        let tm = self.tab_manager.clone();
        let mut tmguard = tm.lock();
        let mut tab_manager = &mut *tmguard;
        
        let tab_len = tab_manager.tabs.len();
        //One tab takes up 6 spaces
        if c < (tab_len * 6) {
            if c % 6 != 0 {
                ret = Some(c/6)
            }
        }

        drop(tmguard);

        return ret;
    }

    fn toggle_selection_wt(
        &mut self,
        ty: SelectionType,
        point: Point,
        side: Side,
        terminal: &mut Term<EventProxy>,
    ) {
        match &mut terminal.selection {
            Some(selection) if selection.ty == ty && !selection.is_empty() => {
                self.clear_selection_wt(terminal);
            },
            Some(selection) if !selection.is_empty() => {
                selection.ty = ty;
                terminal.dirty = true;
            },
            _ => self.start_selection_wt(ty, point, side, terminal),
        }
    }

    fn toggle_selection(&mut self, ty: SelectionType, point: Point, side: Side) {
        tm_cl!(self.tab_manager, terminal, {
            self.toggle_selection_wt(ty, point, side, &mut terminal);
        });
    }

    fn mouse_coords(&self) -> Option<Point> {
        let x = self.mouse.x as usize;
        let y = self.mouse.y as usize;

        if self.size_info.contains_point(x, y) {
            Some(self.size_info.pixels_to_coords(x, y))
        } else {
            None
        }
    }

    #[inline]
    fn mouse_mode_wt(&self, terminal: &mut Term<EventProxy>) -> bool {
        let mut r: bool = true;

        r = terminal.mode().intersects(TermMode::MOUSE_MODE)
            && !terminal.mode().contains(TermMode::VI);

        r
    }

    #[inline]
    fn mouse_mode(&self) -> bool {
        let mut r: bool = true;
        tm_cl!(self.tab_manager, terminal, {
            r = self.mouse_mode_wt(&mut terminal);
        });
        r
    }

    #[inline]
    fn mouse_mut(&mut self) -> &mut Mouse {
        self.mouse
    }

    #[inline]
    fn mouse(&self) -> &Mouse {
        self.mouse
    }

    #[inline]
    fn received_count(&mut self) -> &mut usize {
        &mut self.received_count
    }

    #[inline]
    fn suppress_chars(&mut self) -> &mut bool {
        &mut self.suppress_chars
    }

    #[inline]
    fn modifiers(&mut self) -> &mut ModifiersState {
        &mut self.modifiers
    }

    #[inline]
    fn window(&self) -> &Window {
        self.window
    }

    #[inline]
    fn window_mut(&mut self) -> &mut Window {
        self.window
    }

    // #[inline]
    // fn terminal(&self) -> &Term<EventProxy> {
    //     self.terminal
    // }

    // #[inline]
    // fn terminal_mut(&mut self) -> &mut Term<EventProxy> {
    //     self.terminal
    // }

    fn spawn_new_instance(&mut self) {
        let mut env_args = env::args();
        let alacritty = env_args.next().unwrap();

        #[cfg(unix)]
        let mut args = {
            // Use working directory of controlling process, or fallback to initial shell.
            let mut pid = unsafe { libc::tcgetpgrp(tty::master_fd()) };
            if pid < 0 {
                pid = tty::child_pid();
            }

            #[cfg(not(any(target_os = "macos", target_os = "freebsd")))]
            let link_path = format!("/proc/{}/cwd", pid);
            #[cfg(target_os = "freebsd")]
            let link_path = format!("/compat/linux/proc/{}/cwd", pid);
            #[cfg(not(target_os = "macos"))]
            let cwd = fs::read_link(link_path);
            #[cfg(target_os = "macos")]
            let cwd = macos::proc::cwd(pid);

            // Add the current working directory as parameter.
            cwd.map(|path| vec!["--working-directory".into(), path]).unwrap_or_default()
        };

        #[cfg(not(unix))]
        let mut args: Vec<PathBuf> = Vec::new();

        let working_directory_set = !args.is_empty();

        // Reuse the arguments passed to Alacritty for the new instance.
        while let Some(arg) = env_args.next() {
            // Drop working directory from existing parameters.
            if working_directory_set && arg == "--working-directory" {
                let _ = env_args.next();
                continue;
            }

            args.push(arg.into());
        }

        start_daemon(&alacritty, &args);
    }

    fn launch_url(&self, url: Url) {
        let mut r: bool = true;
        tm_cl!(self.tab_manager, terminal, {
            self.launch_url_wt(url, &mut terminal);
        });
    }

    /// Spawn URL launcher when clicking on URLs.
    fn launch_url_wt(&self, url: Url, terminal: &mut Term<EventProxy>) {
        if self.mouse.block_url_launcher {
            return;
        }

        if let Some(ref launcher) = self.config.ui_config.mouse.url.launcher {
            let mut args = launcher.args().to_vec();
            let start = terminal.visible_to_buffer(url.start());
            let end = terminal.visible_to_buffer(url.end());
            args.push(terminal.bounds_to_string(start, end));

            start_daemon(launcher.program(), &args);
        }
    }

    fn change_font_size(&mut self, delta: f32) {
        tm_cl!(self.tab_manager, terminal, {
            *self.font_size = max(*self.font_size + delta, Size::new(FONT_SIZE_STEP));
            let font = self.config.ui_config.font.clone().with_size(*self.font_size);
            self.display_update_pending.set_font(font);
            terminal.dirty = true;
        });
    }

    fn reset_font_size(&mut self) {
        tm_cl!(self.tab_manager, terminal, {
            *self.font_size = self.config.ui_config.font.size;
            self.display_update_pending.set_font(self.config.ui_config.font.clone());
            terminal.dirty = true;
        });
    }

    #[inline]
    fn pop_message(&mut self) {
        if !self.message_buffer.is_empty() {
            self.display_update_pending.dirty = true;
            self.message_buffer.pop();
        }
    }

    #[inline]
    fn start_search(&mut self, direction: Direction) {
        tm_cl!(self.tab_manager, terminal, {
            self.start_search_wt(direction, &mut terminal);
        });
    }

    #[inline]
    fn start_search_wt(&mut self, direction: Direction, terminal: &mut Term<EventProxy>) {
        let num_lines = terminal.screen_lines();
        let num_cols = terminal.cols();

        self.search_state.focused_match = None;
        self.search_state.regex = Some(String::new());
        self.search_state.direction = direction;

        // Store original search position as origin and reset location.
        self.search_state.display_offset_delta = 0;

        let mut r: Point = Point::new(Line(0), Column(0));

        terminal.dirty = true;

        r = if terminal.mode().contains(TermMode::VI) {
            terminal.vi_mode_cursor.point
        } else {
            match direction {
                Direction::Right => Point::new(Line(0), Column(0)),
                Direction::Left => Point::new(num_lines - 2, num_cols - 1),
            }
        };

        self.search_state.origin = r;

        self.display_update_pending.dirty = true;
    }

    #[inline]
    fn confirm_search(&mut self) {
        tm_cl!(self.tab_manager, terminal, {
            self.confirm_search_wt(&mut terminal);
        });
    }

    #[inline]
    fn confirm_search_wt(&mut self, terminal: &mut Term<EventProxy>) {
        if self.scheduler.scheduled(TimerId::DelayedSearch) {
            self.goto_match(None, terminal);
        }

        self.exit_search_wt(terminal);
    }

    #[inline]
    fn cancel_search(&mut self) {
        tm_cl!(self.tab_manager, terminal, {
            terminal.cancel_search();

            if terminal.mode().contains(TermMode::VI) {
                // Recover pre-search state in vi mode.
                self.search_reset_state(terminal);
            } else if let Some(focused_match) = &self.search_state.focused_match {
                // Create a selection for the focused match.
                let start = terminal.grid().clamp_buffer_to_visible(*focused_match.start());
                let end = terminal.grid().clamp_buffer_to_visible(*focused_match.end());
                self.start_selection_wt(SelectionType::Simple, start, Side::Left, terminal);
                self.update_selection_wt(end, Side::Right, &mut terminal);
            }

            self.exit_search_wt(terminal);
        });
    }

    #[inline]
    fn push_search(&mut self, c: char, terminal: &mut Term<EventProxy>) {
        if let Some(regex) = self.search_state.regex.as_mut() {
            if !terminal.mode().contains(TermMode::VI) {
                // Clear selection so we do not obstruct any matches.
                terminal.selection = None;
            }

            regex.push(c);
            self.update_search_wt(terminal);
        }
    }

    #[inline]
    fn pop_search_wt(&mut self, terminal: &mut Term<EventProxy>) {
        if let Some(regex) = self.search_state.regex.as_mut() {
            regex.pop();
            self.update_search_wt(terminal);
        }
    }

    #[inline]
    fn pop_search(&mut self) {
        tm_cl!(self.tab_manager, terminal, {
            self.pop_search_wt(&mut terminal);
        });
    }

    #[inline]
    fn pop_word_search_wt(&mut self, terminal: &mut Term<EventProxy>) {
        if let Some(regex) = self.search_state.regex.as_mut() {
            *regex = regex.trim_end().to_owned();
            regex.truncate(regex.rfind(' ').map(|i| i + 1).unwrap_or(0));
            self.update_search();
        }
    }

    #[inline]
    fn pop_word_search(&mut self) {
        tm_cl!(self.tab_manager, terminal, {
            self.pop_word_search_wt(&mut terminal);
        });
    }

    #[inline]
    fn advance_search_origin(&mut self, direction: Direction) {
        tm_cl!(self.tab_manager, terminal, {
            let origin = self.absolute_origin(&mut terminal);
            terminal.scroll_to_point(origin);

            // Move the search origin right in front of the next match in the specified direction.
            if let Some(regex_match) = terminal.search_next(origin, direction, Side::Left, None) {
                let origin = match direction {
                    Direction::Right => *regex_match.end(),
                    Direction::Left => {
                        regex_match.start().sub_absolute(terminal, Boundary::Wrap, 1)
                    },
                };
                terminal.scroll_to_point(origin);

                let origin_relative = terminal.grid().clamp_buffer_to_visible(origin);
                self.search_state.origin = origin_relative;
                self.search_state.display_offset_delta = 0;

                self.update_search_wt(terminal);
            }
        });
    }

    /// Handle keyboard typing start.
    ///
    /// This will temporarily disable some features like terminal cursor blinking or the mouse
    /// cursor.
    ///
    /// All features are re-enabled again automatically.
    #[inline]
    fn on_typing_start(&mut self) {
        // Disable cursor blinking.
        let blink_interval = self.config.cursor.blink_interval();
        if let Some(timer) = self.scheduler.get_mut(TimerId::BlinkCursor) {
            timer.deadline = Instant::now() + Duration::from_millis(blink_interval);
            *self.cursor_hidden = false;

            tm_cl!(self.tab_manager, terminal, {
                terminal.dirty = true;
            });
        }

        // Hide mouse cursor.
        if self.config.ui_config.mouse.hide_when_typing {
            self.window.set_mouse_visible(false);
        }
    }

    #[inline]
    fn search_direction(&self) -> Direction {
        self.search_state.direction
    }

    #[inline]
    fn search_active(&self) -> bool {
        self.search_state.regex.is_some()
    }

    fn message(&self) -> Option<&Message> {
        self.message_buffer.message()
    }

    fn config(&self) -> &Config {
        self.config
    }

    fn event_loop(&self) -> &EventLoopWindowTarget<Event> {
        self.event_loop
    }

    fn urls(&self) -> &Urls {
        self.urls
    }

    fn clipboard_mut(&mut self) -> &mut Clipboard {
        self.clipboard
    }

    fn scheduler_mut(&mut self) -> &mut Scheduler {
        self.scheduler
    }
}

impl<'a> ActionContext<'a> {
    fn update_search(&mut self) {
        tm_cl!(self.tab_manager, terminal, {
            self.update_search_wt(terminal);
        });
    }

    fn update_search_wt(&mut self, terminal: &mut Term<EventProxy>) {
        let regex = match self.search_state.regex.as_mut() {
            Some(regex) => regex,
            None => return,
        };

        // Hide cursor while typing into the search bar.
        if self.config.ui_config.mouse.hide_when_typing {
            self.window.set_mouse_visible(false);
        }

        if regex.is_empty() {
            // Stop search if there's nothing to search for.
            self.search_reset_state(terminal);
            terminal.cancel_search();

            if !terminal.mode().contains(TermMode::VI) {
                // Restart search without vi mode to clear the search origin.
                self.start_search_wt(self.search_state.direction, terminal);
            }
        } else {
            // Create terminal search from the new regex string.
            terminal.start_search(&regex);

            // Update search highlighting.
            self.goto_match(MAX_SEARCH_WHILE_TYPING, terminal);
        }

        terminal.dirty = true;
    }

    /// Reset terminal to the state before search was started.
    fn search_reset_state(&mut self, terminal: &mut Term<EventProxy>) {
        // Reset display offset.
        terminal.scroll_display(Scroll::Delta(self.search_state.display_offset_delta));
        self.search_state.display_offset_delta = 0;

        // Clear focused match.
        self.search_state.focused_match = None;

        // Reset vi mode cursor.
        let mut origin = self.search_state.origin;
        origin.line = min(origin.line, terminal.screen_lines() - 1);
        origin.col = min(origin.col, terminal.cols() - 1);
        terminal.vi_mode_cursor.point = origin;

        // Unschedule pending timers.
        self.scheduler.unschedule(TimerId::DelayedSearch);
    }

    /// Jump to the first regex match from the search origin.
    fn goto_match(&mut self, mut limit: Option<usize>, terminal: &mut Term<EventProxy>) {
        let regex = match self.search_state.regex.take() {
            Some(regex) => regex,
            None => return,
        };

        // Limit search only when enough lines are available to run into the limit.
        limit = limit.filter(|&limit| limit <= terminal.total_lines());

        // Jump to the next match.
        let direction = self.search_state.direction;
        let ab_orig = self.absolute_origin(terminal);
        match terminal.search_next(ab_orig, direction, Side::Left, limit) {
            Some(regex_match) => {
                let old_offset = terminal.grid().display_offset() as isize;

                if terminal.mode().contains(TermMode::VI) {
                    // Move vi cursor to the start of the match.
                    terminal.vi_goto_point(*regex_match.start());
                } else {
                    // Select the match when vi mode is not active.
                    terminal.scroll_to_point(*regex_match.start());
                }

                // Update the focused match.
                self.search_state.focused_match = Some(regex_match);

                // Store number of lines the viewport had to be moved.
                let display_offset = terminal.grid().display_offset();
                self.search_state.display_offset_delta += old_offset - display_offset as isize;

                // Since we found a result, we require no delayed re-search.
                self.scheduler.unschedule(TimerId::DelayedSearch);
            },
            // Reset viewport only when we know there is no match, to prevent unnecessary jumping.
            None if limit.is_none() => self.search_reset_state(terminal),
            None => {
                // Schedule delayed search if we ran into our search limit.
                if !self.scheduler.scheduled(TimerId::DelayedSearch) {
                    self.scheduler.schedule(
                        Event::SearchNext.into(),
                        TYPING_SEARCH_DELAY,
                        false,
                        TimerId::DelayedSearch,
                    );
                }

                // Clear focused match.
                self.search_state.focused_match = None;
            },
        }

        self.search_state.regex = Some(regex);
    }

    /// Cleanup the search state.
    fn exit_search(&mut self) {
        tm_cl!(self.tab_manager, terminal, {
            self.exit_search_wt(&mut terminal);
        });
    }

    fn exit_search_wt(&mut self, terminal: &mut Term<EventProxy>) {
        if terminal.history_size() != 0
            && terminal.grid().display_offset() == 0
            && terminal.screen_lines() > terminal.vi_mode_cursor.point.line + 1
        {
            terminal.vi_mode_cursor.point.line += 1;
        }

        self.display_update_pending.dirty = true;
        self.search_state.regex = None;
        terminal.dirty = true;

        // Clear focused match.
        self.search_state.focused_match = None;
    }

    /// Get the absolute position of the search origin.
    ///
    /// This takes the relative motion of the viewport since the start of the search into account.
    /// So while the absolute point of the origin might have changed since new content was printed,
    /// this will still return the correct absolute position.
    fn absolute_origin(&self, terminal: &mut Term<EventProxy>) -> Point<usize> {
        let mut relative_origin = self.search_state.origin;
        relative_origin.line = min(relative_origin.line, terminal.screen_lines() - 1);
        relative_origin.col = min(relative_origin.col, terminal.cols() - 1);
        let mut origin = terminal.visible_to_buffer(relative_origin);
        origin.line = (origin.line as isize + self.search_state.display_offset_delta) as usize;

        return origin;
    }

    /// Update the cursor blinking state.
    fn update_cursor_blinking(&mut self) {
        let mut tab_manager_guard = self.tab_manager.lock();
        let mut tab_manager = &mut *tab_manager_guard;
        let mut tab = tab_manager.selected_tab().unwrap();
        let mut terminal_guard = tab.terminal.lock();
        let mut terminal = &mut *terminal_guard;

        // Get config cursor style.
        let mut cursor_style = self.config.cursor.style;
        if terminal.mode().contains(TermMode::VI) {
            cursor_style = self.config.cursor.vi_mode_style.unwrap_or(cursor_style);
        };

        // Check terminal cursor style.
        let terminal_blinking = terminal.cursor_style().blinking;
        let blinking = cursor_style.blinking_override().unwrap_or(terminal_blinking);

        // Update cursor blinking state.
        self.scheduler.unschedule(TimerId::BlinkCursor);
        if blinking && terminal.is_focused {
            self.scheduler.schedule(
                GlutinEvent::UserEvent(Event::BlinkCursor),
                Duration::from_millis(self.config.cursor.blink_interval()),
                true,
                TimerId::BlinkCursor,
            )
        } else {
            *self.cursor_hidden = false;
            terminal.dirty = true;
        }
        drop(terminal_guard);
        drop(tab_manager_guard);
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum ClickState {
    None,
    Click,
    DoubleClick,
    TripleClick,
}

/// State of the mouse.
#[derive(Debug)]
pub struct Mouse {
    pub x: usize,
    pub y: usize,
    pub left_button_state: ElementState,
    pub middle_button_state: ElementState,
    pub right_button_state: ElementState,
    pub last_click_timestamp: Instant,
    pub last_click_button: MouseButton,
    pub click_state: ClickState,
    pub scroll_px: f64,
    pub line: Line,
    pub column: Column,
    pub cell_side: Side,
    pub lines_scrolled: f32,
    pub block_url_launcher: bool,
    pub inside_text_area: bool,
    pub inside_tab_bar: bool,
}

impl Default for Mouse {
    fn default() -> Mouse {
        Mouse {
            x: 0,
            y: 0,
            last_click_timestamp: Instant::now(),
            last_click_button: MouseButton::Left,
            left_button_state: ElementState::Released,
            middle_button_state: ElementState::Released,
            right_button_state: ElementState::Released,
            click_state: ClickState::None,
            scroll_px: 0.,
            line: Line(0),
            column: Column(0),
            cell_side: Side::Left,
            lines_scrolled: 0.,
            block_url_launcher: false,
            inside_text_area: false,
            inside_tab_bar: false,
        }
    }
}

/// The event processor.
///
/// Stores some state from received events and dispatches actions when they are
/// triggered.
pub struct Processor {
    tab_manager: Arc<FairMutex<TabManager>>,
    mouse: Mouse,
    received_count: usize,
    suppress_chars: bool,
    clipboard: Clipboard,
    modifiers: ModifiersState,
    config: Config,
    message_buffer: MessageBuffer,
    display: Display,
    font_size: Size,
    event_queue: Vec<GlutinEvent<'static, Event>>,
    search_state: SearchState,
    cli_options: CLIOptions,
}

impl Processor {
    /// Create a new event processor.
    ///
    /// Takes a writer which is expected to be hooked up to the write end of a PTY.
    pub fn new(
        tab_manager: Arc<FairMutex<TabManager>>,
        message_buffer: MessageBuffer,
        config: Config,
        display: Display,
        cli_options: CLIOptions,
    ) -> Processor {
        #[cfg(not(any(target_os = "macos", windows)))]
        let clipboard = Clipboard::new(display.window.wayland_display());
        #[cfg(any(target_os = "macos", windows))]
        let clipboard = Clipboard::new();

        Processor {
            tab_manager,
            mouse: Default::default(),
            received_count: 0,
            suppress_chars: false,
            modifiers: Default::default(),
            font_size: config.ui_config.font.size,
            config,
            message_buffer,
            display,
            event_queue: Vec::new(),
            clipboard,
            search_state: SearchState::new(),
            cli_options,
        }
    }

    /// Return `true` if `event_queue` is empty, `false` otherwise.
    #[inline]
    #[cfg(all(feature = "wayland", not(any(target_os = "macos", windows))))]
    fn event_queue_empty(&mut self) -> bool {
        let wayland_event_queue = match self.display.wayland_event_queue.as_mut() {
            Some(wayland_event_queue) => wayland_event_queue,
            // Since frame callbacks do not exist on X11, just check for event queue.
            None => return self.event_queue.is_empty(),
        };

        // Check for pending frame callbacks on Wayland.
        let events_dispatched = wayland_event_queue
            .dispatch_pending(&mut (), |_, _, _| {})
            .expect("failed to dispatch event queue");

        self.event_queue.is_empty() && events_dispatched == 0
    }

    /// Return `true` if `event_queue` is empty, `false` otherwise.
    #[inline]
    #[cfg(any(not(feature = "wayland"), target_os = "macos", windows))]
    fn event_queue_empty(&mut self) -> bool {
        self.event_queue.is_empty()
    }

    /// Run the event loop.
    pub fn run(&mut self, mut event_loop: EventLoop<Event>) {
        let mut scheduler = Scheduler::new();

        // Start the initial cursor blinking timer.
        if self.config.cursor.style().blinking {
            let event: Event = TerminalEvent::CursorBlinkingChange(true).into();
            self.event_queue.push(event.into());
        }

        let tab_manager_mutex_clone = self.tab_manager.clone();

        let mut last = Instant::now();
        let ld = Duration::from_millis(500);

        event_loop.run_return(move |event, event_loop, control_flow| {
            if self.config.ui_config.debug.print_events {
                info!("glutin event: {:?}", event);
            }

            // Ignore all events we do not care about.
            if Self::skip_event(&event) {
                return;
            }

            let mut display_update_pending = DisplayUpdate::default();

            match event {
                // Check for shutdown.
                GlutinEvent::UserEvent(Event::TerminalEvent(TerminalEvent::Exit)) => {
                    *control_flow = ControlFlow::Exit;
                    return;
                },

                GlutinEvent::UserEvent(Event::TestEvent) => {

                },
                // Process events.
                GlutinEvent::RedrawEventsCleared => {
                    *control_flow = match scheduler.update(&mut self.event_queue) {
                        Some(instant) => ControlFlow::WaitUntil(instant),
                        None => ControlFlow::Wait,
                    };

                    if self.event_queue_empty() {
                        return;
                    }
                },
                // Remap DPR change event to remove lifetime.
                GlutinEvent::WindowEvent {
                    event: WindowEvent::ScaleFactorChanged { scale_factor, new_inner_size },
                    ..
                } => {
                    *control_flow = ControlFlow::Poll;
                    let size = (new_inner_size.width, new_inner_size.height);
                    self.event_queue.push(Event::DPRChanged(scale_factor, size).into());
                    return;
                },
                // Transmute to extend lifetime, which exists only for `ScaleFactorChanged` event.
                // Since we remap that event to remove the lifetime, this is safe.
                event => unsafe {
                    *control_flow = ControlFlow::Poll;
                    self.event_queue.push(mem::transmute(event));
                    return;
                },
            }

            let old_is_searching = self.search_state.regex.is_some();

            let context = ActionContext {
                tab_manager: self.tab_manager.clone(),
                mouse: &mut self.mouse,
                clipboard: &mut self.clipboard,
                size_info: &mut self.display.size_info,
                received_count: &mut self.received_count,
                suppress_chars: &mut self.suppress_chars,
                modifiers: &mut self.modifiers,
                message_buffer: &mut self.message_buffer,
                display_update_pending: &mut display_update_pending,
                window: &mut self.display.window,
                font_size: &mut self.font_size,
                config: &mut self.config,
                urls: &self.display.urls,
                scheduler: &mut scheduler,
                search_state: &mut self.search_state,
                cli_options: &self.cli_options,
                cursor_hidden: &mut self.display.cursor_hidden,
                event_loop,
            };
            let mut processor = input::Processor::new(context, &self.display.highlighted_url);

            for event in self.event_queue.drain(..) {
                let tab_manager_event_processing_clone = tab_manager_mutex_clone.clone();
                Processor::handle_event(event, &mut processor, tab_manager_event_processing_clone);
            }

            
            let tm = tab_manager_mutex_clone.clone();
            let mut tab_manager_guard = tm.lock();
            let tab_manager: &mut TabManager = &mut *tab_manager_guard;
            let tab = tab_manager.selected_tab().unwrap();
            let terminal_mutex = tab.terminal.clone();
            let mut terminal_guard = terminal_mutex.lock();
            let mut terminal = &mut *terminal_guard;

            let terminal_dirty = terminal.dirty;
            let terminal_visual_bell_completed = terminal.visual_bell.completed();

            drop(terminal_guard);
            drop(tab_manager_guard);
            

            // Process DisplayUpdate events.
            if display_update_pending.dirty {
                self.submit_display_update(
                    old_is_searching,
                    display_update_pending,
                );
            }

            // Skip rendering on Wayland until we get frame event from compositor.
            #[cfg(not(any(target_os = "macos", windows)))]
            if !self.display.is_x11 && !self.display.window.should_draw.load(Ordering::Relaxed)
            {
                return;
            }

            // if last + ld < Instant::now() {
            if terminal_dirty {
                // last = Instant::now();
                // terminal.dirty = false;

                // Request immediate re-draw if visual bell animation is not finished yet.
                if !terminal_visual_bell_completed {
                    let event: Event = TerminalEvent::Wakeup.into();
                    self.event_queue.push(event.into());

                    *control_flow = ControlFlow::Poll;
                }

                let tm = tab_manager_mutex_clone.clone();
                let mut tab_manager_guard = tm.lock();
                let tab_manager: &mut TabManager = &mut *tab_manager_guard;
                let tab = tab_manager.selected_tab().unwrap();
                let terminal_mutex = tab.terminal.clone();
                let mut terminal_guard = terminal_mutex.lock();
                let mut terminal = &mut *terminal_guard;
                // Redraw screen.
                self.display.draw(
                    tab_manager,
                    terminal,
                    &self.message_buffer,
                    &self.config,
                    &self.mouse,
                    self.modifiers,
                    &self.search_state,
                );

                drop(tab_manager_guard);
                drop(terminal_guard);
            }

            // println!("Before run drop");
        });

        // // Write ref tests to disk.
        // TODO: Uncomment?
        // if self.config.ui_config.debug.ref_test {
        //     self.write_ref_test_results(&terminal.lock());
        // }
    }

    /// Handle events from glutin.
    ///
    /// Doesn't take self mutably due to borrow checking.
    fn handle_event(
        event: GlutinEvent<'_, Event>,
        processor: &mut input::Processor<'_, EventProxy, ActionContext<'_>>,
        // terminal: &mut Term<EventProxy>
        tab_manager_mutex: Arc<FairMutex<TabManager>>,
    ) {
        match event {
            GlutinEvent::UserEvent(event) => match event {
                Event::TestEvent => {
                    // println!("Another handler for test event");
                },
                Event::DPRChanged(scale_factor, (width, height)) => {
                    let display_update_pending = &mut processor.ctx.display_update_pending;

                    // Push current font to update its DPR.
                    let font = processor.ctx.config.ui_config.font.clone();
                    display_update_pending.set_font(font.with_size(*processor.ctx.font_size));

                    // Resize to event's dimensions, since no resize event is emitted on Wayland.
                    display_update_pending.set_dimensions(PhysicalSize::new(width, height));

                    processor.ctx.window.dpr = scale_factor;

                    tm_cl!(tab_manager_mutex, terminal, {
                        terminal.dirty = true;
                    });
                },
                Event::Message(message) => {
                    processor.ctx.message_buffer.push(message);
                    processor.ctx.display_update_pending.dirty = true;

                    tm_cl!(tab_manager_mutex, terminal, {
                        terminal.dirty = true;
                    });
                },
                Event::SearchNext => {
                    tm_cl!(tab_manager_mutex, terminal, {
                        processor.ctx.goto_match(None, &mut terminal);
                    });
                },
                Event::ConfigReload(path) => Self::reload_config(&path, processor),
                Event::Scroll(scroll) => processor.ctx.scroll(scroll),
                Event::BlinkCursor => {
                    *processor.ctx.cursor_hidden ^= true;

                    tm_cl!(tab_manager_mutex, terminal, {
                        terminal.dirty = true;
                    });
                },
                Event::TerminalEvent(event) => match event {
                    TerminalEvent::Title(title) => {
                        let ui_config = &processor.ctx.config.ui_config;
                        if ui_config.dynamic_title() {
                            processor.ctx.window.set_title(&title);
                        }
                    },
                    TerminalEvent::ResetTitle => {
                        let ui_config = &processor.ctx.config.ui_config;
                        if ui_config.dynamic_title() {
                            processor.ctx.window.set_title(&ui_config.window.title);
                        }
                    },
                    TerminalEvent::Wakeup => {
                        tm_cl!(tab_manager_mutex, terminal, {
                            terminal.dirty = true;
                        });
                    },
                    TerminalEvent::Close(idx) => {
                        let tm = tab_manager_mutex.clone();
                        let mut tab_manager_guard = tm.lock();
                        let tab_manager: &mut TabManager = &mut *tab_manager_guard;

                        //Note: no longer attempting to tie the tab_idx to the tab being closed
                        tab_manager.remove_selected_tab();
                        
                        drop(tab_manager_guard);
                    },
                    TerminalEvent::Bell => {
                        let bell_command = processor.ctx.config.bell().command.as_ref();
                        let _ = bell_command.map(|cmd| start_daemon(cmd.program(), cmd.args()));

                        tm_cl!(tab_manager_mutex, terminal, {
                            if terminal.mode().contains(TermMode::URGENCY_HINTS) {
                                processor.ctx.window.set_urgent(!terminal.is_focused);
                            }
                        });
                    },
                    TerminalEvent::ClipboardStore(clipboard_type, content) => {
                        processor.ctx.clipboard.store(clipboard_type, content);
                    },
                    TerminalEvent::ClipboardLoad(clipboard_type, format) => {
                        let text = format(processor.ctx.clipboard.load(clipboard_type).as_str());
                        processor.ctx.write_to_pty(text.into_bytes());
                    },
                    TerminalEvent::MouseCursorDirty => processor.reset_mouse_cursor(),
                    TerminalEvent::Exit => (),
                    TerminalEvent::CursorBlinkingChange(_) => {
                        processor.ctx.update_cursor_blinking();
                    },
                },
            },
            GlutinEvent::RedrawRequested(_) => {
                tm_cl!(tab_manager_mutex, terminal, {
                    terminal.dirty = true;
                });
            },
            GlutinEvent::WindowEvent { event, window_id, .. } => {
                match event {
                    WindowEvent::CloseRequested => {
                        tm_cl!(tab_manager_mutex, terminal, {
                            terminal.exit();
                        });
                    },
                    WindowEvent::Resized(size) => {
                        // Minimizing the window sends a Resize event with zero width and
                        // height. But there's no need to ever actually resize to this.
                        // Both WinPTY & ConPTY have issues when resizing down to zero size
                        // and back.
                        #[cfg(windows)]
                        if size.width == 0 && size.height == 0 {
                            return;
                        }

                        processor.ctx.display_update_pending.set_dimensions(size);

                        tm_cl!(tab_manager_mutex, terminal, {
                            terminal.dirty = true;
                        });
                    },
                    WindowEvent::KeyboardInput { input, is_synthetic: false, .. } => {
                        processor.key_input(input);
                    },
                    WindowEvent::ReceivedCharacter(c) => processor.received_char(c),
                    WindowEvent::MouseInput { state, button, .. } => {
                        processor.ctx.window.set_mouse_visible(true);
                        processor.mouse_input(state, button);

                        tm_cl!(tab_manager_mutex, terminal, {
                            terminal.dirty = true;
                        });
                    },
                    WindowEvent::ModifiersChanged(modifiers) => {
                        processor.modifiers_input(modifiers)
                    },
                    WindowEvent::CursorMoved { position, .. } => {
                        processor.ctx.window.set_mouse_visible(true);
                        processor.mouse_moved(position);
                    },
                    WindowEvent::MouseWheel { delta, phase, .. } => {
                        processor.ctx.window.set_mouse_visible(true);
                        processor.mouse_wheel_input(delta, phase);
                    },
                    WindowEvent::Focused(is_focused) => {
                        if window_id == processor.ctx.window.window_id() {
                            // processor.ctx.terminal.is_focused = is_focused;

                            tm_cl!(tab_manager_mutex, terminal, {
                                terminal.is_focused = is_focused;
                                terminal.dirty = true;
                            });

                            if is_focused {
                                processor.ctx.window.set_urgent(false);
                            } else {
                                processor.ctx.window.set_mouse_visible(true);
                            }

                            processor.ctx.update_cursor_blinking();
                            processor.on_focus_change(is_focused);
                        }
                    },
                    WindowEvent::DroppedFile(path) => {
                        let path: String = path.to_string_lossy().into();
                        processor.ctx.write_to_pty((path + " ").into_bytes());
                    },
                    WindowEvent::CursorLeft { .. } => {
                        processor.ctx.mouse.inside_text_area = false;

                        if processor.highlighted_url.is_some() {
                            tm_cl!(tab_manager_mutex, terminal, {
                                terminal.dirty = true;
                            });
                        }
                    },
                    WindowEvent::KeyboardInput { is_synthetic: true, .. }
                    | WindowEvent::TouchpadPressure { .. }
                    | WindowEvent::ScaleFactorChanged { .. }
                    | WindowEvent::CursorEntered { .. }
                    | WindowEvent::AxisMotion { .. }
                    | WindowEvent::HoveredFileCancelled
                    | WindowEvent::Destroyed
                    | WindowEvent::ThemeChanged(_)
                    | WindowEvent::HoveredFile(_)
                    | WindowEvent::Touch(_)
                    | WindowEvent::Moved(_) => (),
                }
            },
            GlutinEvent::Suspended { .. }
            | GlutinEvent::NewEvents { .. }
            | GlutinEvent::DeviceEvent { .. }
            | GlutinEvent::MainEventsCleared
            | GlutinEvent::RedrawEventsCleared
            | GlutinEvent::Resumed
            | GlutinEvent::LoopDestroyed => (),
        }
    }

    /// Check if an event is irrelevant and can be skipped.
    fn skip_event(event: &GlutinEvent<'_, Event>) -> bool {
        match event {
            GlutinEvent::WindowEvent { event, .. } => matches!(
                event,
                WindowEvent::KeyboardInput { is_synthetic: true, .. }
                    | WindowEvent::TouchpadPressure { .. }
                    | WindowEvent::CursorEntered { .. }
                    | WindowEvent::AxisMotion { .. }
                    | WindowEvent::HoveredFileCancelled
                    | WindowEvent::Destroyed
                    | WindowEvent::HoveredFile(_)
                    | WindowEvent::Touch(_)
                    | WindowEvent::Moved(_)
            ),
            GlutinEvent::Suspended { .. }
            | GlutinEvent::NewEvents { .. }
            | GlutinEvent::MainEventsCleared
            | GlutinEvent::LoopDestroyed => true,
            _ => false,
        }
    }

    fn reload_config(
        path: &PathBuf,
        processor: &mut input::Processor<'_, EventProxy, ActionContext<'_>>,
    ) {
        if !processor.ctx.message_buffer.is_empty() {
            processor.ctx.message_buffer.remove_target(LOG_TARGET_CONFIG);
            processor.ctx.display_update_pending.dirty = true;
        }

        let config = match config::reload(&path, &processor.ctx.cli_options) {
            Ok(config) => config,
            Err(_) => return,
        };

        // processor.ctx.terminal.update_config(&config);
        processor.ctx.with_terminal(|terminal: &mut Term<EventProxy>| {
            terminal.update_config(&config);
        });

        // Reload cursor if its thickness has changed.
        if (processor.ctx.config.cursor.thickness() - config.cursor.thickness()).abs()
            > std::f64::EPSILON
        {
            processor.ctx.display_update_pending.set_cursor_dirty();
        }

        if processor.ctx.config.ui_config.font != config.ui_config.font {
            // Do not update font size if it has been changed at runtime.
            if *processor.ctx.font_size == processor.ctx.config.ui_config.font.size {
                *processor.ctx.font_size = config.ui_config.font.size;
            }

            let font = config.ui_config.font.clone().with_size(*processor.ctx.font_size);
            processor.ctx.display_update_pending.set_font(font);
        }

        // Update display if padding options were changed.
        let window_config = &processor.ctx.config.ui_config.window;
        if window_config.padding(1.) != config.ui_config.window.padding(1.)
            || window_config.dynamic_padding != config.ui_config.window.dynamic_padding
        {
            processor.ctx.display_update_pending.dirty = true;
        }

        // Live title reload.
        if !config.ui_config.dynamic_title()
            || processor.ctx.config.ui_config.window.title != config.ui_config.window.title
        {
            processor.ctx.window.set_title(&config.ui_config.window.title);
        }

        #[cfg(all(feature = "wayland", not(any(target_os = "macos", windows))))]
        if processor.ctx.event_loop.is_wayland() {
            processor.ctx.window.set_wayland_theme(&config.colors);
        }

        // Set subpixel anti-aliasing.
        #[cfg(target_os = "macos")]
        set_font_smoothing(config.ui_config.font.use_thin_strokes());

        *processor.ctx.config = config;

        // Update cursor blinking.
        processor.ctx.update_cursor_blinking();

        processor.ctx.with_terminal(|terminal: &mut Term<EventProxy>| terminal.dirty = true);
    }

    /// Submit the pending changes to the `Display`.
    fn submit_display_update(
        &mut self,
        old_is_searching: bool,
        display_update_pending: DisplayUpdate,
    ) {
        // Compute cursor positions before resize.
        let tm = self.tab_manager.clone();
        let mut tab_manager_guard = tm.lock();
        let tab_manager: &mut TabManager = &mut *tab_manager_guard;
        let tab = tab_manager.selected_tab().unwrap();
        let terminal_mutex = tab.terminal.clone();
        let mut terminal_guard = terminal_mutex.lock();
        let mut terminal = &mut *terminal_guard;

        let num_lines = terminal.screen_lines();
        let cursor_at_bottom = terminal.grid().cursor.point.line + 1 == num_lines;
        let origin_at_bottom = if terminal.mode().contains(TermMode::VI) {
            terminal.vi_mode_cursor.point.line == num_lines - 1
        } else {
            self.search_state.direction == Direction::Left
        };

        drop(tab_manager_guard);
        drop(terminal_guard);

        self.display.handle_update(
            &self.message_buffer,
            self.search_state.regex.is_some(),
            &self.config,
            display_update_pending,
        );

        let tm = self.tab_manager.clone();
        let mut tab_manager_guard = tm.lock();
        let tab_manager: &mut TabManager = &mut *tab_manager_guard;
        let tab = tab_manager.selected_tab().unwrap();
        let terminal_mutex = tab.terminal.clone();
        let mut terminal_guard = terminal_mutex.lock();
        let mut terminal = &mut *terminal_guard;

        // Scroll to make sure search origin is visible and content moves as little as possible.
        if !old_is_searching && self.search_state.regex.is_some() {
            let display_offset = terminal.grid().display_offset();
            if display_offset == 0 && cursor_at_bottom && !origin_at_bottom {
                terminal.scroll_display(Scroll::Delta(1));
            } else if display_offset != 0 && origin_at_bottom {
                terminal.scroll_display(Scroll::Delta(-1));
            }
        }

        drop(tab_manager_guard);
        drop(terminal_guard);
    }

    /// Write the ref test results to the disk.
    fn write_ref_test_results(&self, terminal: &Term<EventProxy>) {
        // Dump grid state.
        let mut grid = terminal.grid().clone();
        grid.initialize_all();
        grid.truncate();

        let serialized_grid = json::to_string(&grid).expect("serialize grid");

        let serialized_size = json::to_string(&self.display.size_info).expect("serialize size");

        let serialized_config = format!("{{\"history_size\":{}}}", grid.history_size());

        File::create("./grid.json")
            .and_then(|mut f| f.write_all(serialized_grid.as_bytes()))
            .expect("write grid.json");

        File::create("./size.json")
            .and_then(|mut f| f.write_all(serialized_size.as_bytes()))
            .expect("write size.json");

        File::create("./config.json")
            .and_then(|mut f| f.write_all(serialized_config.as_bytes()))
            .expect("write config.json");
    }
}

#[derive(Debug, Clone)]
pub struct EventProxy(EventLoopProxy<Event>);

impl EventProxy {
    pub fn new(proxy: EventLoopProxy<Event>) -> Self {
        EventProxy(proxy)
    }

    /// Send an event to the event loop.
    pub fn send_event(&self, event: Event) {
        let _ = self.0.send_event(event);
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: TerminalEvent) {
        let _ = self.0.send_event(Event::TerminalEvent(event));
    }
}
