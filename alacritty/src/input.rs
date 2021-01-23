//! Handle input from glutin.
//!
//! Certain key combinations should send some escape sequence back to the PTY.
//! In order to figure that out, state about which modifier keys are pressed
//! needs to be tracked. Additionally, we need a bit of a state machine to
//! determine what to do when a non-modifier key is pressed.

use crate::{clipboard, tab_manager::TabManager};
use alacritty_terminal::sync::FairMutex;
use log::trace;
use std::borrow::Cow;
use std::cmp::{max, min, Ordering};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use glutin::dpi::PhysicalPosition;
use glutin::event::{
    ElementState, KeyboardInput, ModifiersState, MouseButton, MouseScrollDelta, TouchPhase,
};
use glutin::event_loop::EventLoopWindowTarget;
#[cfg(target_os = "macos")]
use glutin::platform::macos::EventLoopWindowTargetExtMacOS;
use glutin::window::CursorIcon;

use crate::event::EventProxy;

use alacritty_terminal::ansi::{ClearMode, Handler};
use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Boundary, Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::SelectionType;
use alacritty_terminal::term::{ClipboardType, SizeInfo, Term, TermMode};
use alacritty_terminal::vi_mode::ViMotion;

use crate::clipboard::Clipboard;
use crate::config::{Action, Binding, BindingMode, Config, Key, SearchAction, ViAction};
use crate::daemon::start_daemon;
use crate::event::{ClickState, Event, Mouse, TYPING_SEARCH_DELAY};
use crate::message_bar::{self, Message};
use crate::scheduler::{Scheduler, TimerId};
use crate::url::{Url, Urls};
use crate::window::Window;

#[macro_use]
use crate::macros;

/// Font size change interval.
pub const FONT_SIZE_STEP: f32 = 0.5;

/// Interval for mouse scrolling during selection outside of the boundaries.
const SELECTION_SCROLLING_INTERVAL: Duration = Duration::from_millis(15);

/// Minimum number of pixels at the bottom/top where selection scrolling is performed.
const MIN_SELECTION_SCROLLING_HEIGHT: f64 = 5.;

/// Number of pixels for increasing the selection scrolling speed factor by one.
const SELECTION_SCROLLING_STEP: f64 = 20.;

/// Processes input from glutin.
///
/// An escape sequence may be emitted in case specific keys or key combinations
/// are activated.
pub struct Processor<'a, T: EventListener, A: ActionContext<T>> {
    pub ctx: A,
    pub highlighted_url: &'a Option<Url>,
    _phantom: PhantomData<T>,
}

pub trait ActionContext<T: EventListener> {
    fn tab_manager(&self) -> Arc<RwLock<TabManager>>;
    fn write_to_pty<B: Into<Cow<'static, [u8]>>>(&mut self, data: B);
    fn size_info(&self) -> SizeInfo;
    fn copy_selection(&mut self, ty: ClipboardType);

    fn find_tab(&self, col: Column) -> Option<usize>;
    fn find_word(&mut self, point: Point, side: Side) -> String;
    fn start_selection_wt(&mut self, ty: SelectionType, point: Point, side: Side, terminal: &mut Term<EventProxy>);
    fn start_selection(&mut self, ty: SelectionType, point: Point, side: Side);
    fn toggle_selection_wt(&mut self, ty: SelectionType, point: Point, side: Side, terminal: &mut Term<EventProxy>);
    fn toggle_selection(&mut self, ty: SelectionType, point: Point, side: Side);
    fn update_selection(&mut self, point: Point, side: Side);
    fn update_selection_wt(&mut self, point: Point, side: Side, terminal: &mut Term<EventProxy>);
    fn clear_selection(&mut self);
    fn clear_selection_wt(&mut self, terminal: &mut Term<EventProxy>);
    fn selection_is_empty(&self) -> bool;
    fn selection_is_empty_wt(&self, terminal: &mut Term<EventProxy>) -> bool;
    fn mouse_mut(&mut self) -> &mut Mouse;
    fn mouse(&self) -> &Mouse;
    fn mouse_coords(&self) -> Option<Point>;
    fn received_count(&mut self) -> &mut usize;
    fn suppress_chars(&mut self) -> &mut bool;
    fn modifiers(&mut self) -> &mut ModifiersState;
    fn scroll(&mut self, scroll: Scroll);
    fn window(&self) -> &Window;
    fn window_mut(&mut self) -> &mut Window;
    // fn terminal(&self) -> &Term<T>;
    // fn terminal_mut(&mut self) -> &mut Term<T>;
    fn spawn_new_instance(&mut self);
    fn change_font_size(&mut self, delta: f32);
    fn reset_font_size(&mut self);
    fn pop_message(&mut self);
    fn message(&self) -> Option<&Message>;
    fn config(&self) -> &Config;
    fn event_loop(&self) -> &EventLoopWindowTarget<Event>;
    fn urls(&self) -> &Urls;
    fn launch_url(&self, url: Url);
    fn launch_url_wt(&self, url: Url, terminal: &mut Term<EventProxy>);
    fn mouse_mode_wt(&self, terminal: &mut Term<EventProxy>) -> bool;

    fn mouse_mode(&self) -> bool;
    fn clipboard_mut(&mut self) -> &mut Clipboard;
    fn scheduler_mut(&mut self) -> &mut Scheduler;
    fn start_search(&mut self, direction: Direction);

    fn start_search_wt(&mut self, direction: Direction, terminal: &mut Term<EventProxy>);
    fn confirm_search(&mut self);

    fn confirm_search_wt(&mut self, terminal: &mut Term<EventProxy>);
    fn cancel_search(&mut self);
    fn search_input(&mut self, c: char);
    fn search_input_wt(&mut self, c: char, terminal: &mut Term<EventProxy>);
    fn search_pop_word(&mut self);
    fn search_history_previous(&mut self);
    fn search_history_next(&mut self);
    fn toggle_grep_mode(&mut self);


    fn set_grep_mode_to(&mut self, to_val: bool);
    fn advance_search_origin(&mut self, direction: Direction);
    fn search_direction(&self) -> Direction;
    fn search_active(&self) -> bool;
    fn on_typing_start(&mut self);
}

trait Execute<T: EventListener> {
    fn execute<A: ActionContext<T>>(&self, ctx: &mut A);
}

impl<T, U: EventListener> Execute<U> for Binding<T> {
    /// Execute the action associate with this binding.
    #[inline]
    fn execute<A: ActionContext<U>>(&self, ctx: &mut A) {
        self.action.execute(ctx)
    }
}

impl Action {
    fn toggle_selection<T, A>(ctx: &mut A, ty: SelectionType)
    where
        T: EventListener,
        A: ActionContext<T>,
    {
        tm_cl!(ctx.tab_manager(), terminal, {

        let cursor_point = terminal.vi_mode_cursor.point;
        ctx.toggle_selection_wt(ty, cursor_point, Side::Left, &mut terminal);

        // Make sure initial selection is not empty.
        if let Some(selection) = &mut terminal.selection {
            selection.include_all();
        }

        });
    }
    
}

impl<T: EventListener> Execute<T> for Action {
    #[inline]
    fn execute<A: ActionContext<T>>(&self, ctx: &mut A) {
        match *self {
            Action::NewTab => {
                let tbarc = ctx.tab_manager().clone();
                let tbg = tbarc.read().unwrap();
                let tab_manager = & *tbg;
                let idx = tab_manager.new_tab().unwrap();
                tab_manager.select_tab(idx);
                // println!("New tab is {}", idx);

                let mut tab = &*tab_manager.selected_tab_arc();
                let mut terminal_guard = tab.terminal.lock();
                let mut terminal = &mut *terminal_guard;
            
                terminal.dirty = true;
                drop(terminal_guard);
                drop(tbg);
            },
            Action::PreviousTab => {
                let tbarc = ctx.tab_manager().clone();
                let tbg = tbarc.read().unwrap();
                let tab_manager = & *tbg;
                let prev_tab = tab_manager.prev_tab_idx().unwrap();
                // println!("prev tab is {}", prev_tab);
                tab_manager.select_tab(prev_tab);
                let mut tab = &*tab_manager.selected_tab_arc();
                let mut terminal_guard = tab.terminal.lock();
                let mut terminal = &mut *terminal_guard;
                terminal.dirty = true;
                drop(terminal_guard);
                drop(tbg);
            },            
            Action::NextTab => {
                let tbarc = ctx.tab_manager().clone();
                let tbg = tbarc.read().unwrap();
                let tab_manager = & *tbg;
                let next_tab = tab_manager.next_tab_idx().unwrap();
                // println!("next tab is {}", next_tab);
                tab_manager.select_tab(next_tab);

                let mut tab = &*tab_manager.selected_tab_arc();
                let mut terminal_guard = tab.terminal.lock();
                let mut terminal = &mut *terminal_guard;
            
                terminal.dirty = true;

                drop(terminal_guard);
                
                drop(tbg);
            },
            Action::Esc(ref s) => {
                // println!("Escape seq being written: {}", s);
                ctx.on_typing_start();

                ctx.clear_selection();
                ctx.scroll(Scroll::Bottom);

                tm_cl!(ctx.tab_manager(), terminal, {

                terminal.dirty = true;
                
                });
                
                ctx.write_to_pty(s.clone().into_bytes())
            },
            Action::Command(ref program) => {
                let args = program.args();
                let program = program.program();
                trace!("Running command {} with args {:?}", program, args);

                start_daemon(program, args);
            },
            Action::ToggleViMode => {
                tm_cl!(ctx.tab_manager(), terminal, {
                    terminal.toggle_vi_mode();
                });
            },
            Action::ViMotion(motion) => {
                ctx.on_typing_start();
                tm_cl!(ctx.tab_manager(), terminal, {
                terminal.vi_motion(motion);
                });
            },
            Action::ViAction(ViAction::ToggleNormalSelection) => {
                Self::toggle_selection(ctx, SelectionType::Simple)
            },
            Action::ViAction(ViAction::ToggleLineSelection) => {
                Self::toggle_selection(ctx, SelectionType::Lines)
            },
            Action::ViAction(ViAction::ToggleBlockSelection) => {
                Self::toggle_selection(ctx, SelectionType::Block)
            },
            Action::ViAction(ViAction::ToggleSemanticSelection) => {
                Self::toggle_selection(ctx, SelectionType::Semantic)
            },
            Action::ViAction(ViAction::Open) => {
                tm_cl!(ctx.tab_manager(), terminal, {

                ctx.mouse_mut().block_url_launcher = false;
                if let Some(url) = ctx.urls().find_at(terminal.vi_mode_cursor.point) {
                    ctx.launch_url_wt(url, &mut terminal);
                }

                });
            },
            Action::ViAction(ViAction::SearchNext) => {
                tm_cl!(ctx.tab_manager(), terminal, {

                let direction = ctx.search_direction();
                let vi_point = terminal.visible_to_buffer(terminal.vi_mode_cursor.point);
                let origin = match direction {
                    Direction::Right => vi_point.add_absolute(terminal, Boundary::Wrap, 1),
                    Direction::Left => vi_point.sub_absolute(terminal, Boundary::Wrap, 1),
                };

                let regex_match = terminal.search_next(origin, direction, Side::Left, None);
                if let Some(regex_match) = regex_match {
                    terminal.vi_goto_point(*regex_match.start());
                }
                });
            },
            Action::ViAction(ViAction::SearchPrevious) => {
                tm_cl!(ctx.tab_manager(), terminal, {

                let direction = ctx.search_direction().opposite();
                let vi_point = terminal.visible_to_buffer(terminal.vi_mode_cursor.point);
                let origin = match direction {
                    Direction::Right => vi_point.add_absolute(terminal, Boundary::Wrap, 1),
                    Direction::Left => vi_point.sub_absolute(terminal, Boundary::Wrap, 1),
                };

                let regex_match = terminal.search_next(origin, direction, Side::Left, None);
                if let Some(regex_match) = regex_match {
                    terminal.vi_goto_point(*regex_match.start());
                }
                });
            },
            Action::ViAction(ViAction::SearchStart) => {
                tm_cl!(ctx.tab_manager(), terminal, {
                
                let origin = terminal
                    .visible_to_buffer(terminal.vi_mode_cursor.point)
                    .sub_absolute(terminal, Boundary::Wrap, 1);
                let regex_match = terminal.search_next(origin, Direction::Left, Side::Left, None);
                if let Some(regex_match) = regex_match {
                    terminal.vi_goto_point(*regex_match.start());
                }
                });
            },
            Action::ViAction(ViAction::SearchEnd) => {
                tm_cl!(ctx.tab_manager(), terminal, {
                    
                    let origin = terminal
                        .visible_to_buffer(terminal.vi_mode_cursor.point)
                        .add_absolute(terminal, Boundary::Wrap, 1);
                let regex_match = terminal.search_next(origin, Direction::Right, Side::Right, None);
                if let Some(regex_match) = regex_match {
                    terminal.vi_goto_point(*regex_match.end());
                }
                });
            },
            Action::SearchAction(SearchAction::SearchFocusNext) => {
                ctx.advance_search_origin(ctx.search_direction());
            },
            Action::SearchAction(SearchAction::GrepMode) => {
                ctx.toggle_grep_mode();
            },
            Action::SearchAction(SearchAction::SearchFocusPrevious) => {
                let direction = ctx.search_direction().opposite();
                ctx.advance_search_origin(direction);
            },
            Action::SearchAction(SearchAction::SearchConfirm) => ctx.confirm_search(),
            Action::SearchAction(SearchAction::SearchCancel) => ctx.cancel_search(),
            Action::SearchAction(SearchAction::SearchClear) => {
                let direction = ctx.search_direction();
                ctx.cancel_search();
                ctx.start_search(direction);
            },
            Action::SearchAction(SearchAction::SearchDeleteWord) => ctx.search_pop_word(),
            Action::SearchAction(SearchAction::SearchHistoryPrevious) => {
                ctx.search_history_previous()
            },
            Action::SearchAction(SearchAction::SearchHistoryNext) => ctx.search_history_next(),
            Action::SearchForward => ctx.start_search(Direction::Right),
            Action::SearchBackward => ctx.start_search(Direction::Left),
            Action::Copy => ctx.copy_selection(ClipboardType::Clipboard),
            #[cfg(not(any(target_os = "macos", windows)))]
            Action::CopySelection => ctx.copy_selection(ClipboardType::Selection),
            Action::ClearSelection => ctx.clear_selection(),
            Action::Paste => {
                let text = ctx.clipboard_mut().load(ClipboardType::Clipboard);
                paste(ctx, &text);
            },
            Action::PasteSelection => {
                let text = ctx.clipboard_mut().load(ClipboardType::Selection);
                paste(ctx, &text);
            },
            Action::ToggleFullscreen => ctx.window_mut().toggle_fullscreen(),
            #[cfg(target_os = "macos")]
            Action::ToggleSimpleFullscreen => ctx.window_mut().toggle_simple_fullscreen(),
            #[cfg(target_os = "macos")]
            Action::Hide => ctx.event_loop().hide_application(),
            #[cfg(not(target_os = "macos"))]
            Action::Hide => ctx.window().set_visible(false),
            Action::Minimize => ctx.window().set_minimized(true),
            Action::Quit => {
                tm_cl!(ctx.tab_manager(), terminal, {

                terminal.exit();
                });
            },
            Action::IncreaseFontSize => ctx.change_font_size(FONT_SIZE_STEP),
            Action::DecreaseFontSize => ctx.change_font_size(FONT_SIZE_STEP * -1.),
            Action::ResetFontSize => ctx.reset_font_size(),
            Action::ScrollPageUp => {
                // Move vi mode cursor.
                tm_cl!(ctx.tab_manager(), terminal, {

                let scroll_lines = terminal.screen_lines().0 as isize;
                terminal.vi_mode_cursor = terminal.vi_mode_cursor.scroll(terminal, scroll_lines);

                ctx.scroll(Scroll::PageUp);
                });
            },
            Action::ScrollPageDown => {
                // Move vi mode cursor.
                tm_cl!(ctx.tab_manager(), terminal, {

                let scroll_lines = -(terminal.screen_lines().0 as isize);
                terminal.vi_mode_cursor = terminal.vi_mode_cursor.scroll(terminal, scroll_lines);

                ctx.scroll(Scroll::PageDown);
                });
            },
            Action::ScrollHalfPageUp => {
                // Move vi mode cursor.
                tm_cl!(ctx.tab_manager(), terminal, {

                let scroll_lines = terminal.screen_lines().0 as isize / 2;
                terminal.vi_mode_cursor = terminal.vi_mode_cursor.scroll(terminal, scroll_lines);

                ctx.scroll(Scroll::Delta(scroll_lines));
                });
            },
            Action::ScrollHalfPageDown => {
                // Move vi mode cursor.
                tm_cl!(ctx.tab_manager(), terminal, {

                let scroll_lines = -(terminal.screen_lines().0 as isize / 2);
                terminal.vi_mode_cursor = terminal.vi_mode_cursor.scroll(terminal, scroll_lines);

                ctx.scroll(Scroll::Delta(scroll_lines));
                });
            },
            Action::ScrollLineUp => {
                // Move vi mode cursor.
                tm_cl!(ctx.tab_manager(), terminal, {

                if terminal.grid().display_offset() != terminal.history_size()
                    && terminal.vi_mode_cursor.point.line + 1 != terminal.screen_lines()
                {
                    terminal.vi_mode_cursor.point.line += 1;
                }

                ctx.scroll(Scroll::Delta(1));
                });
            },
            Action::ScrollLineDown => {
                // Move vi mode cursor.
                tm_cl!(ctx.tab_manager(), terminal, {

                if terminal.grid().display_offset() != 0
                    && terminal.vi_mode_cursor.point.line.0 != 0
                {
                    terminal.vi_mode_cursor.point.line -= 1;
                }

                ctx.scroll(Scroll::Delta(-1));
                });
            },
            Action::ScrollToTop => {
                tm_cl!(ctx.tab_manager(), terminal, {

                ctx.scroll(Scroll::Top);

                // Move vi mode cursor.
                terminal.vi_mode_cursor.point.line = Line(0);
                terminal.vi_motion(ViMotion::FirstOccupied);
                });
            },
            Action::ScrollToBottom => {
                tm_cl!(ctx.tab_manager(), terminal, {

                ctx.scroll(Scroll::Bottom);

                // Move vi mode cursor.

                terminal.vi_mode_cursor.point.line = terminal.screen_lines() - 1;

                // Move to beginning twice, to always jump across linewraps.
                terminal.vi_motion(ViMotion::FirstOccupied);
                terminal.vi_motion(ViMotion::FirstOccupied);
                });
            },
            Action::ClearHistory => {
                tm_cl!(ctx.tab_manager(), terminal, {

                terminal.clear_screen(ClearMode::Saved);
                });
            },
            Action::ClearLogNotice => ctx.pop_message(),
            Action::SpawnNewInstance => ctx.spawn_new_instance(),
            Action::ReceiveChar | Action::None => (),
        }
    }
}

fn paste<T: EventListener, A: ActionContext<T>>(ctx: &mut A, contents: &str) {
    tm_cl!(ctx.tab_manager(), terminal, {
    let is_brack_paste = terminal.mode().contains(TermMode::BRACKETED_PASTE);
    });
    
    if ctx.search_active() {
        for c in contents.chars() {
            ctx.search_input(c);
        }
    } else if is_brack_paste {
        ctx.write_to_pty(&b"\x1b[200~"[..]);
        ctx.write_to_pty(contents.replace("\x1b", "").into_bytes());
        ctx.write_to_pty(&b"\x1b[201~"[..]);
    } else {
        // In non-bracketed (ie: normal) mode, terminal applications cannot distinguish
        // pasted data from keystrokes.
        // In theory, we should construct the keystrokes needed to produce the data we are
        // pasting... since that's neither practical nor sensible (and probably an impossible
        // task to solve in a general way), we'll just replace line breaks (windows and unix
        // style) with a single carriage return (\r, which is what the Enter key produces).
        ctx.write_to_pty(contents.replace("\r\n", "\r").replace("\n", "\r").into_bytes());
    }

}

#[derive(Debug, Clone, PartialEq)]
pub enum MouseState {
    Url(Url),
    MessageBar,
    MessageBarButton,
    TabBarButton,
    Mouse,
    Text,
}

impl From<MouseState> for CursorIcon {
    fn from(mouse_state: MouseState) -> CursorIcon {
        match mouse_state {
            MouseState::Url(_) | MouseState::MessageBarButton | MouseState::TabBarButton => CursorIcon::Hand,
            MouseState::Text => CursorIcon::Text,
            _ => CursorIcon::Default,
        }
    }
}

impl<'a, T: EventListener, A: ActionContext<T>> Processor<'a, T, A> {
    pub fn new(ctx: A, highlighted_url: &'a Option<Url>) -> Self {
        Self { ctx, highlighted_url, _phantom: Default::default() }
    }

    #[inline]
    pub fn mouse_moved(&mut self, position: PhysicalPosition<f64>) {
        

        let size_info = self.ctx.size_info();

        let (x, y) = position.into();

        let lmb_pressed = self.ctx.mouse().left_button_state == ElementState::Pressed;
        let rmb_pressed = self.ctx.mouse().right_button_state == ElementState::Pressed;

        tm_cl!(self.ctx.tab_manager(), terminal, {
            if !self.ctx.selection_is_empty_wt(&mut terminal) && (lmb_pressed || rmb_pressed) {
                self.update_selection_scrolling(y);
            }
        });

        let x = min(max(x, 0), size_info.width() as i32 - 1) as usize;
        let y = min(max(y, 0), size_info.height() as i32 - 1) as usize;

        self.ctx.mouse_mut().x = x;
        self.ctx.mouse_mut().y = y;

        let inside_text_area = size_info.contains_point(x, y);
        let point = size_info.pixels_to_coords(x, y);
        let cell_side = self.get_mouse_side();

        let cell_changed =
            point.line != self.ctx.mouse().line || point.col != self.ctx.mouse().column;

        // Update mouse state and check for URL change.
        let mouse_state = self.mouse_state();

        tm_cl!(self.ctx.tab_manager(), terminal, {
            self.update_url_state(&mouse_state, &mut terminal);
        });
        self.ctx.window_mut().set_mouse_cursor(mouse_state.into());

        // If the mouse hasn't changed cells, do nothing.
        if !cell_changed
            && self.ctx.mouse().cell_side == cell_side
            && self.ctx.mouse().inside_text_area == inside_text_area
        {
            return;
        }

        let point_tab_bar = size_info.pixels_to_coords_including_tab_bar(x, y);
        if point_tab_bar.line.0 < 1 {
            self.ctx.mouse_mut().inside_tab_bar = true;    
        } else {
            self.ctx.mouse_mut().inside_tab_bar = false;    
        }

        self.ctx.mouse_mut().inside_text_area = inside_text_area;
        self.ctx.mouse_mut().cell_side = cell_side;
        self.ctx.mouse_mut().line = point.line;
        self.ctx.mouse_mut().column = point.col;

        // Don't launch URLs if mouse has moved.
        self.ctx.mouse_mut().block_url_launcher = true;
    tm_cl!(self.ctx.tab_manager(), terminal, {
        
        if (lmb_pressed || rmb_pressed) && (self.ctx.modifiers().shift() || !self.ctx.mouse_mode_wt(terminal))
        {
            self.ctx.update_selection_wt(point, cell_side, terminal);
        } else if cell_changed
            && point.line < terminal.screen_lines()
            && terminal.mode().intersects(TermMode::MOUSE_MOTION | TermMode::MOUSE_DRAG)
        {
            if lmb_pressed {
                self.mouse_report_wt(32, ElementState::Pressed, terminal);
            } else if self.ctx.mouse().middle_button_state == ElementState::Pressed {
                self.mouse_report_wt(33, ElementState::Pressed, terminal);
            } else if self.ctx.mouse().right_button_state == ElementState::Pressed {
                self.mouse_report_wt(34, ElementState::Pressed, terminal);
            } else if terminal.mode().contains(TermMode::MOUSE_MOTION) {
                self.mouse_report_wt(35, ElementState::Pressed, terminal);
            }
        }

        });
    }

    fn get_mouse_side(&self) -> Side {
        let size_info = self.ctx.size_info();
        let x = self.ctx.mouse().x;

        let cell_x =
            x.saturating_sub(size_info.padding_x() as usize) % size_info.cell_width() as usize;
        let half_cell_width = (size_info.cell_width() / 2.0) as usize;

        let additional_padding =
            (size_info.width() - size_info.padding_x() * 2.) % size_info.cell_width();
        let end_of_grid = size_info.width() - size_info.padding_x() - additional_padding;

        if cell_x > half_cell_width
            // Edge case when mouse leaves the window.
            || x as f32 >= end_of_grid
        {
            Side::Right
        } else {
            Side::Left
        }
    }

    fn normal_mouse_report(&mut self, button: u8) {
        
        let (line, column) = (self.ctx.mouse().line, self.ctx.mouse().column);
        tm_cl!(self.ctx.tab_manager(), terminal, {
            let utf8 = terminal.mode().contains(TermMode::UTF8_MOUSE);
        });

        let max_point = if utf8 { 2015 } else { 223 };

        if line >= Line(max_point) || column >= Column(max_point) {
            return;
        }

        let mut msg = vec![b'\x1b', b'[', b'M', 32 + button];

        let mouse_pos_encode = |pos: usize| -> Vec<u8> {
            let pos = 32 + 1 + pos;
            let first = 0xC0 + pos / 64;
            let second = 0x80 + (pos & 63);
            vec![first as u8, second as u8]
        };

        if utf8 && column >= Column(95) {
            msg.append(&mut mouse_pos_encode(column.0));
        } else {
            msg.push(32 + 1 + column.0 as u8);
        }

        if utf8 && line >= Line(95) {
            msg.append(&mut mouse_pos_encode(line.0));
        } else {
            msg.push(32 + 1 + line.0 as u8);
        }
        
        self.ctx.write_to_pty(msg);;
    }

    fn sgr_mouse_report(&mut self, button: u8, state: ElementState) {
        let (line, column) = (self.ctx.mouse().line, self.ctx.mouse().column);
        let c = match state {
            ElementState::Pressed => 'M',
            ElementState::Released => 'm',
        };

        let msg = format!("\x1b[<{};{};{}{}", button, column + 1, line + 1, c);
        self.ctx.write_to_pty(msg.into_bytes());
    }

    fn mouse_report(&mut self, button: u8, state: ElementState) {
        // Calculate modifiers value.
        let mut mods = 0;
        let modifiers = self.ctx.modifiers();
        if modifiers.shift() {
            mods += 4;
        }
        if modifiers.alt() {
            mods += 8;
        }
        if modifiers.ctrl() {
            mods += 16;
        }

        tm_cl!(self.ctx.tab_manager(), terminal, {
            let sgr_mouse = terminal.mode().contains(TermMode::SGR_MOUSE);
        });

        // Report mouse events.
        if sgr_mouse {
            self.sgr_mouse_report(button + mods, state);
        } else if let ElementState::Released = state {
            self.normal_mouse_report(3 + mods);
        } else {
            self.normal_mouse_report(button + mods);
        }
    }

    fn mouse_report_wt(&mut self, button: u8, state: ElementState, terminal: &mut Term<EventProxy>) {
        // Calculate modifiers value.
        let mut mods = 0;
        let modifiers = self.ctx.modifiers();
        if modifiers.shift() {
            mods += 4;
        }
        if modifiers.alt() {
            mods += 8;
        }
        if modifiers.ctrl() {
            mods += 16;
        }

        // Report mouse events.
        if terminal.mode().contains(TermMode::SGR_MOUSE) {
            self.sgr_mouse_report(button + mods, state);
        } else if let ElementState::Released = state {
            self.normal_mouse_report(3 + mods);
        } else {
            self.normal_mouse_report(button + mods);
        }
    }

    fn on_mouse_press(&mut self, button: MouseButton) {
        // Handle mouse mode.
        tm_cl!(self.ctx.tab_manager(), terminal, {
        let shift_and_mode = !self.ctx.modifiers().shift() && self.ctx.mouse_mode_wt(&mut terminal);
        });

        if shift_and_mode {
            self.ctx.mouse_mut().click_state = ClickState::None;

            let code = match button {
                MouseButton::Left => 0,
                MouseButton::Middle => 1,
                MouseButton::Right => 2,
                // Can't properly report more than three buttons..
                MouseButton::Other(_) => return,
            };

            
            self.mouse_report(code, ElementState::Pressed);
            
        } else {
            // Calculate time since the last click to handle double/triple clicks.
            let now = Instant::now();
            let elapsed = now - self.ctx.mouse().last_click_timestamp;
            self.ctx.mouse_mut().last_click_timestamp = now;

            // Update multi-click state.
            let mouse_config = &self.ctx.config().ui_config.mouse;
            self.ctx.mouse_mut().click_state = match self.ctx.mouse().click_state {
                // Reset click state if button has changed.
                _ if button != self.ctx.mouse().last_click_button => {
                    self.ctx.mouse_mut().last_click_button = button;
                    ClickState::Click
                },
                ClickState::Click if elapsed < mouse_config.double_click.threshold() => {
                    ClickState::DoubleClick
                },
                ClickState::DoubleClick if elapsed < mouse_config.triple_click.threshold() => {
                    ClickState::TripleClick
                },
                _ => ClickState::Click,
            };

            // Load mouse point, treating message bar and padding as the closest cell.
            let mouse = self.ctx.mouse();

            tm_cl!(self.ctx.tab_manager(), terminal, {
                let mut point = self.ctx.size_info().pixels_to_coords(mouse.x, mouse.y);
                point.line = min(point.line, terminal.screen_lines() - 1);
            });

            match button {
                MouseButton::Left => self.on_left_click(point),
                MouseButton::Right => self.on_right_click(point),
                // Do nothing when using buttons other than LMB.
                _ => self.ctx.mouse_mut().click_state = ClickState::None,
            }
        }

    }

    /// Handle selection expansion on right click.
    fn on_right_click(&mut self, point: Point) {
        match self.ctx.mouse().click_state {
            ClickState::Click => {
                let selection_type = if self.ctx.modifiers().ctrl() {
                    SelectionType::Block
                } else {
                    SelectionType::Simple
                };

                self.expand_selection(point, selection_type);
            },
            ClickState::DoubleClick => self.expand_selection(point, SelectionType::Semantic),
            ClickState::TripleClick => self.expand_selection(point, SelectionType::Lines),
            ClickState::None => (),
        }
    }

    /// Expand existing selection.
    fn expand_selection(&mut self, point: Point, selection_type: SelectionType) {
        tm_cl!(self.ctx.tab_manager(), terminal, {

        let cell_side = self.ctx.mouse().cell_side;

        let selection = match &mut terminal.selection {
            Some(selection) => selection,
            None => return,
        };

        selection.ty = selection_type;
        self.ctx.update_selection_wt(point, cell_side, &mut terminal);

        // Move vi mode cursor to mouse click position.
        if terminal.mode().contains(TermMode::VI) && !self.ctx.search_active() {
            terminal.vi_mode_cursor.point = point;
        }
        });
    }

    /// Handle left click selection and vi mode cursor movement.
    fn on_left_click(&mut self, point: Point) {
        tm_cl!(self.ctx.tab_manager(), terminal, {

        let side = self.ctx.mouse().cell_side;

        match self.ctx.mouse().click_state {
            ClickState::Click => {
                // Don't launch URLs if this click cleared the selection.
                self.ctx.mouse_mut().block_url_launcher =
                    !self.ctx.selection_is_empty_wt(&mut terminal);

                self.ctx.clear_selection_wt(&mut terminal);

                // Start new empty selection.
                if self.ctx.modifiers().ctrl() {
                    self.ctx.start_selection_wt(SelectionType::Block, point, side, &mut terminal);
                } else {
                    self.ctx.start_selection_wt(SelectionType::Simple, point, side, &mut terminal);
                }
            },
            ClickState::DoubleClick => {
                self.ctx.mouse_mut().block_url_launcher = true;
                self.ctx.start_selection_wt(SelectionType::Semantic, point, side, &mut terminal);
            },
            ClickState::TripleClick => {
                self.ctx.mouse_mut().block_url_launcher = true;
                self.ctx.start_selection_wt(SelectionType::Lines, point, side, &mut terminal);
            },
            ClickState::None => (),
        };

        // Move vi mode cursor to mouse click position.
        if terminal.mode().contains(TermMode::VI) && !self.ctx.search_active() {
            terminal.vi_mode_cursor.point = point;
        }

        });
    }

    fn on_mouse_release(&mut self, button: MouseButton) {
        tm_cl!(self.ctx.tab_manager(), terminal, {
            let mouse_mode_if = !self.ctx.modifiers().shift() && self.ctx.mouse_mode_wt(&mut terminal);
        });

        if mouse_mode_if {
            let code = match button {
                MouseButton::Left => 0,
                MouseButton::Middle => 1,
                MouseButton::Right => 2,
                // Can't properly report more than three buttons.
                MouseButton::Other(_) => return,
            };
            
            self.mouse_report(code, ElementState::Released);
            
            return;
        } else if let (MouseButton::Left, MouseState::Url(url)) = (button, self.mouse_state()) {
            tm_cl!(self.ctx.tab_manager(), terminal, {
                self.ctx.launch_url_wt(url, &mut terminal);
            });
        }

        self.ctx.scheduler_mut().unschedule(TimerId::SelectionScrolling);
        self.copy_selection();

    }

    pub fn mouse_wheel_input(&mut self, delta: MouseScrollDelta, phase: TouchPhase) {
        match delta {
            MouseScrollDelta::LineDelta(_columns, lines) => {
                let new_scroll_px = lines * self.ctx.size_info().cell_height();
                self.scroll_terminal(f64::from(new_scroll_px));
            },
            MouseScrollDelta::PixelDelta(lpos) => {
                match phase {
                    TouchPhase::Started => {
                        // Reset offset to zero.
                        self.ctx.mouse_mut().scroll_px = 0.;
                    },
                    TouchPhase::Moved => {
                        self.scroll_terminal(lpos.y);
                    },
                    _ => (),
                }
            },
        }
    }

    fn scroll_terminal(&mut self, new_scroll_px: f64) {
        tm_cl!(self.ctx.tab_manager(), terminal, {

        let mouse_mode = self.ctx.mouse_mode_wt(&mut terminal);
        let term_mode = terminal.mode().clone();
        let term_vi_mode_point = terminal.vi_mode_cursor.point;
        let selection_not_empty = !self.ctx.selection_is_empty_wt(&mut terminal);

        });

        let height = f64::from(self.ctx.size_info().cell_height());

        if mouse_mode {
            self.ctx.mouse_mut().scroll_px += new_scroll_px;

            let code = if new_scroll_px > 0. { 64 } else { 65 };
            let lines = (self.ctx.mouse().scroll_px / height).abs() as i32;

            
            for _ in 0..lines {
                self.mouse_report(code, ElementState::Pressed);
            }
            
        } else if term_mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
            && !self.ctx.modifiers().shift()
        {
            let multiplier = f64::from(self.ctx.config().scrolling.multiplier);
            self.ctx.mouse_mut().scroll_px += new_scroll_px * multiplier;

            let cmd = if new_scroll_px > 0. { b'A' } else { b'B' };
            let lines = (self.ctx.mouse().scroll_px / height).abs() as i32;

            let mut content = Vec::with_capacity(lines as usize * 3);
            for _ in 0..lines {
                content.push(0x1b);
                content.push(b'O');
                content.push(cmd);
            }
            self.ctx.write_to_pty(content);
        } else {
            let multiplier = f64::from(self.ctx.config().scrolling.multiplier);
            self.ctx.mouse_mut().scroll_px += new_scroll_px * multiplier;

            let lines = self.ctx.mouse().scroll_px / height;

            // Store absolute position of vi mode cursor.

            tm_cl!(self.ctx.tab_manager(), terminal, {
                let absolute = terminal.visible_to_buffer(terminal.vi_mode_cursor.point);
            });

            self.ctx.scroll(Scroll::Delta(lines as isize));

            // Try to restore vi mode cursor position, to keep it above its previous content.

            tm_cl!(self.ctx.tab_manager(), terminal, {
                terminal.vi_mode_cursor.point = terminal.grid().clamp_buffer_to_visible(absolute);
                terminal.vi_mode_cursor.point.col = absolute.col;

                // Update selection.
                if term_mode.contains(TermMode::VI) {
                    if selection_not_empty {
                        self.ctx.update_selection_wt(terminal.vi_mode_cursor.point, Side::Right, &mut terminal);
                    }
                }
            });
        }

        self.ctx.mouse_mut().scroll_px %= height;

    }

    pub fn on_focus_change(&mut self, is_focused: bool) {
        tm_cl!(self.ctx.tab_manager(), terminal, {
            let is_focus = terminal.mode().contains(TermMode::FOCUS_IN_OUT);
        });
        if is_focus {
            let chr = if is_focused { "I" } else { "O" };

            let msg = format!("\x1b[{}", chr);
            self.ctx.write_to_pty(msg.into_bytes());
        }
    }

    pub fn mouse_input(&mut self, state: ElementState, button: MouseButton) {
        match button {
            MouseButton::Left => self.ctx.mouse_mut().left_button_state = state,
            MouseButton::Middle => self.ctx.mouse_mut().middle_button_state = state,
            MouseButton::Right => self.ctx.mouse_mut().right_button_state = state,
            _ => (),
        }

        // Skip normal mouse events if the message bar has been clicked.
        if self.ctx.mouse().line.0 == 0 && state == ElementState::Pressed {
            match self.ctx.find_tab(self.ctx.mouse().column) {
                Some(tab_idx) => {
                    let tm = self.ctx.tab_manager().clone();
                    let tmguard = tm.read().unwrap();
                    let tab_manager = & *tmguard;
                    tab_manager.select_tab(tab_idx);
                    drop(tmguard);
                },
                None => {

                }
            }
        } else if self.message_or_tab_bar_mouse_state() == Some(MouseState::MessageBarButton)
            && state == ElementState::Pressed
        {
            let size = self.ctx.size_info();

            let current_lines = self.ctx.message().map(|m| m.text(&size).len()).unwrap_or(0);

            self.ctx.clear_selection();
            self.ctx.pop_message();

            // Reset cursor when message bar height changed or all messages are gone.
            let new_lines = self.ctx.message().map(|m| m.text(&size).len()).unwrap_or(0);

            let new_icon = match current_lines.cmp(&new_lines) {
                Ordering::Less => CursorIcon::Default,
                Ordering::Equal => CursorIcon::Hand,
                Ordering::Greater => {
                    if self.ctx.mouse_mode() {
                        CursorIcon::Default
                    } else {
                        CursorIcon::Text
                    }
                },
            };

            self.ctx.window_mut().set_mouse_cursor(new_icon);
        } else {
            match state {
                ElementState::Pressed => {
                    self.process_mouse_bindings(button);
                    self.on_mouse_press(button);
                },
                ElementState::Released => self.on_mouse_release(button),
            }

            if button == MouseButton::Left && self.ctx.modifiers().logo() {
                let mouse = self.ctx.mouse();
                let mut point = self.ctx.size_info().pixels_to_coords(mouse.x, mouse.y);
                let side = self.ctx.mouse().cell_side;
                let word_clicked = self.ctx.find_word(point, side);

                if state == ElementState::Pressed {
                    self.ctx.write_to_pty(word_clicked.as_bytes().to_vec());
                }

            }

            if button == MouseButton::Left && self.ctx.modifiers().alt() {
                if state == ElementState::Released {
                    self.ctx.copy_selection(alacritty_terminal::term::ClipboardType::Selection);
                    self.ctx.clear_selection();
                }
            }
        }
    }

    /// Process key input.
    pub fn key_input(&mut self, input: KeyboardInput) {
        // Reset search delay when the user is still typing.
        if self.ctx.search_active() {
            if let Some(timer) = self.ctx.scheduler_mut().get_mut(TimerId::DelayedSearch) {
                timer.deadline = Instant::now() + TYPING_SEARCH_DELAY;
            }
        }

        match input.state {
            ElementState::Pressed => {
                *self.ctx.received_count() = 0;
                self.process_key_bindings(input);
            },
            ElementState::Released => *self.ctx.suppress_chars() = false,
        }
    }

    /// Modifier state change.
    pub fn modifiers_input(&mut self, modifiers: ModifiersState) {
        *self.ctx.modifiers() = modifiers;

        
        // Update mouse state and check for URL change.
        let mouse_state = self.mouse_state();
        tm_cl!(self.ctx.tab_manager(), terminal, {
            self.update_url_state(&mouse_state, &mut terminal);
        });
        self.ctx.window_mut().set_mouse_cursor(mouse_state.into());
    }

    /// Reset mouse cursor based on modifier and terminal state.
    #[inline]
    pub fn reset_mouse_cursor(&mut self) {
        let mouse_state = self.mouse_state();
        self.ctx.window_mut().set_mouse_cursor(mouse_state.into());
    }

    /// Process a received character.
    pub fn received_char(&mut self, c: char) {
        // println!("\nreceived char: {}\n", c);
        let suppress_chars = *self.ctx.suppress_chars();
        let search_active = self.ctx.search_active();
        tm_cl!(self.ctx.tab_manager(), terminal, {
            let is_vi_mode = terminal.mode().contains(TermMode::VI);
        });
        if suppress_chars || is_vi_mode || search_active {
            if search_active && !suppress_chars {
                self.ctx.search_input(c);
            }
            return;
        }
        

        self.ctx.on_typing_start();

        self.ctx.scroll(Scroll::Bottom);
        self.ctx.clear_selection();

        let utf8_len = c.len_utf8();
        let mut bytes = Vec::with_capacity(utf8_len);
        unsafe {
            bytes.set_len(utf8_len);
            c.encode_utf8(&mut bytes[..]);
        }

        if self.ctx.config().ui_config.alt_send_esc
            && *self.ctx.received_count() == 0
            && self.ctx.modifiers().alt()
            && utf8_len == 1
        {
            bytes.insert(0, b'\x1b');
        }

        self.ctx.write_to_pty(bytes);

        *self.ctx.received_count() += 1;
    }

    /// Attempt to find a binding and execute its action.
    ///
    /// The provided mode, mods, and key must match what is allowed by a binding
    /// for its action to be executed.
    fn process_key_bindings(&mut self, input: KeyboardInput) {
        tm_cl!(self.ctx.tab_manager(), terminal, {
            let mode = BindingMode::new(terminal.mode(), self.ctx.search_active());
        });
        let mods = *self.ctx.modifiers();
        let mut suppress_chars = None;

        for i in 0..self.ctx.config().ui_config.key_bindings().len() {
            let binding = &self.ctx.config().ui_config.key_bindings()[i];

            let key = match (binding.trigger, input.virtual_keycode) {
                (Key::Scancode(_), _) => Key::Scancode(input.scancode),
                (_, Some(key)) => Key::Keycode(key),
                _ => continue,
            };

            if binding.is_triggered_by(mode, mods, &key) {
                // Binding was triggered; run the action.
                let binding = binding.clone();
                binding.execute(&mut self.ctx);

                // Pass through the key if any of the bindings has the `ReceiveChar` action.
                *suppress_chars.get_or_insert(true) &= binding.action != Action::ReceiveChar;
            }
            // binding.execute(&mut self.ctx);
        }

        // Don't suppress char if no bindings were triggered.
        *self.ctx.suppress_chars() = suppress_chars.unwrap_or(false);
    }

    /// Attempt to find a binding and execute its action.
    ///
    /// The provided mode, mods, and key must match what is allowed by a binding
    /// for its action to be executed.
    fn process_mouse_bindings(&mut self, button: MouseButton) {
        tm_cl!(self.ctx.tab_manager(), terminal, {
            let mode = BindingMode::new(terminal.mode(), self.ctx.search_active());
        });
        let mouse_mode = self.ctx.mouse_mode();
        let mods = *self.ctx.modifiers();

        for i in 0..self.ctx.config().ui_config.mouse_bindings().len() {
            let mut binding = self.ctx.config().ui_config.mouse_bindings()[i].clone();

            // Require shift for all modifiers when mouse mode is active.
            if mouse_mode {
                binding.mods |= ModifiersState::SHIFT;
            }

            if binding.is_triggered_by(mode, mods, &button) {
                binding.execute(&mut self.ctx);
            }
        }
    }

    /// Check mouse state in relation to the message bar.
    fn message_or_tab_bar_mouse_state(&self) -> Option<MouseState> {
        let mouse = self.ctx.mouse();
        
        if mouse.inside_tab_bar {
            match self.ctx.find_tab(mouse.column) {
                Some(tab_idx) => {
                    Some(MouseState::TabBarButton)
                }, 
                None => {
                    None
                }
            }
        } else {
            // Since search is above the message bar, the button is offset by search's height.
            let search_height = if self.ctx.search_active() { 1 } else { 0 };

            // Calculate Y position of the end of the last terminal line.
            let size = self.ctx.size_info();
            let terminal_end = size.padding_y() as usize
                + size.cell_height() as usize * (size.screen_lines().0 + search_height);

            
            if self.ctx.message().is_none() || (mouse.y <= terminal_end) {
                None
            } else if mouse.y <= terminal_end - size.cell_height() as usize
                && mouse.column + message_bar::CLOSE_BUTTON_TEXT.len() >= size.cols()
            {
                Some(MouseState::MessageBarButton)
            } else {
                Some(MouseState::MessageBar)
            }
        }
    }

    /// Copy text selection.
    fn copy_selection(&mut self) {
        if self.ctx.config().selection.save_to_clipboard {
            self.ctx.copy_selection(ClipboardType::Clipboard);
        }
        self.ctx.copy_selection(ClipboardType::Selection);
    }

    /// Trigger redraw when URL highlight changed.
    #[inline]
    fn update_url_state(&mut self, mouse_state: &MouseState, terminal: &mut Term<EventProxy>) {
        if let MouseState::Url(url) = mouse_state {
            if Some(url) != self.highlighted_url.as_ref() {
                terminal.dirty = true;
            }
        } else if self.highlighted_url.is_some() {
            terminal.dirty = true;
        }
    }

    /// Location of the mouse cursor.
    fn mouse_state(&mut self) -> MouseState {
        // Check message bar before URL to ignore URLs in the message bar.
        if let Some(mouse_state) = self.message_or_tab_bar_mouse_state() {
            return mouse_state;
        }

        tm_cl!(self.ctx.tab_manager(), terminal, {
            let mouse_mode = self.ctx.mouse_mode_wt(terminal);

            // Check for URL at mouse cursor.
            let mods = *self.ctx.modifiers();
            let highlighted_url = self.ctx.urls().highlighted(
                self.ctx.config(),
                self.ctx.mouse(),
                mods,
                mouse_mode,
                !self.ctx.selection_is_empty_wt(terminal),
            );

            if let Some(url) = highlighted_url {
                return MouseState::Url(url);
            }

        });
        // Check mouse mode if location is not special.
        if !self.ctx.modifiers().shift() && mouse_mode {
            MouseState::Mouse
        } else {
            MouseState::Text
        }
    }
    

    /// Handle automatic scrolling when selecting above/below the window.

    fn update_selection_scrolling(&mut self, mouse_y: i32) {
        let dpr = self.ctx.window().dpr;
        let size = self.ctx.size_info();
        let scheduler = self.ctx.scheduler_mut();

        // Scale constants by DPI.
        let min_height = (MIN_SELECTION_SCROLLING_HEIGHT * dpr) as i32;
        let step = (SELECTION_SCROLLING_STEP * dpr) as i32;

        // Compute the height of the scrolling areas.
        let end_top = max(min_height, size.padding_y() as i32);
        let text_area_bottom = size.padding_y() + size.screen_lines().0 as f32 * size.cell_height();
        let start_bottom = min(size.height() as i32 - min_height, text_area_bottom as i32);

        // Get distance from closest window boundary.
        let delta = if mouse_y < end_top {
            end_top - mouse_y + step
        } else if mouse_y >= start_bottom {
            start_bottom - mouse_y - step
        } else {
            scheduler.unschedule(TimerId::SelectionScrolling);
            return;
        };

        // Scale number of lines scrolled based on distance to boundary.
        let delta = delta as isize / step as isize;
        let event = Event::Scroll(Scroll::Delta(delta));

        // Schedule event.
        match scheduler.get_mut(TimerId::SelectionScrolling) {
            Some(timer) => timer.event = event.into(),
            None => {
                scheduler.schedule(
                    event.into(),
                    SELECTION_SCROLLING_INTERVAL,
                    true,
                    TimerId::SelectionScrolling,
                );
            },
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    use glutin::event::{Event as GlutinEvent, VirtualKeyCode, WindowEvent};

    use alacritty_terminal::event::Event as TerminalEvent;
    use alacritty_terminal::selection::Selection;

    use crate::message_bar::MessageBuffer;

    const KEY: VirtualKeyCode = VirtualKeyCode::Key0;

    struct MockEventProxy;

    impl EventListener for MockEventProxy {
        fn send_event(&self, _event: TerminalEvent) {}
    }

    struct ActionContext<'a, T> {
        pub tab_mananger: Arc<RwLock<TabManager>>,
        // pub terminal: &'a mut Term<T>,
        pub selection: &'a mut Option<Selection>,
        pub size_info: &'a SizeInfo,
        pub mouse: &'a mut Mouse,
        pub clipboard: &'a mut Clipboard,
        pub message_buffer: &'a mut MessageBuffer,
        pub received_count: usize,
        pub suppress_chars: bool,
        pub modifiers: ModifiersState,
        config: &'a Config,
    }

    impl<'a, T: EventListener> super::ActionContext<T> for ActionContext<'a, T> {

        
        fn write_to_pty<B: Into<Cow<'static, [u8]>>>(&mut self, _val: B) {}

        fn update_selection(&mut self, _point: Point, _side: Side) {}

        fn update_selection_wt(&mut self, _point: Point, _side: Side, terminal: &mut Term<EventProxy>) {}

        fn start_selection_wt(&mut self, ty: SelectionType, point: Point, side: Side, terminal: &mut Term<EventProxy>) {}
        fn start_selection(&mut self, _ty: SelectionType, _point: Point, _side: Side) {}

        fn toggle_selection(&mut self, _ty: SelectionType, _point: Point, _side: Side) {}

        fn copy_selection(&mut self, _: ClipboardType) {}

        fn clear_selection(&mut self) {}

        fn spawn_new_instance(&mut self) {}

        fn change_font_size(&mut self, _delta: f32) {}

        fn reset_font_size(&mut self) {}

        fn start_search(&mut self, _direction: Direction) {}

        fn start_search_wt(&mut self, direction: Direction, terminal: &mut Term<EventProxy>) {}

        fn confirm_search(&mut self) {}

        fn confirm_search_wt(&mut self, terminal: &mut Term<EventProxy>) {}

        fn cancel_search(&mut self) {}

        fn search_input(&mut self, _c: char) {}

        fn search_input_wt(&mut self, c: char, terminal: &mut Term<EventProxy>) {}

        fn search_pop_word(&mut self) {}

        fn search_history_previous(&mut self) {}

        fn search_history_next(&mut self) {}

        fn toggle_grep_mode(&mut self) {}

        fn set_grep_mode_to(&mut self, to_val: bool) {}


        fn advance_search_origin(&mut self, _direction: Direction) {}

        fn search_direction(&self) -> Direction {
            Direction::Right
        }

        fn search_active(&self) -> bool {
            false
        }

        fn find_tab(&self, col: Column) -> Option<usize> {
            None
        }

        fn find_word(&mut self, point: Point, side: Side) -> String {
            ""
        }

        fn toggle_selection_wt(&mut self, ty: SelectionType, point: Point, side: Side, terminal: &mut Term<EventProxy>) {
            
        }

        fn clear_selection_wt(&mut self, terminal: &mut Term<EventProxy>) {

        }
        fn selection_is_empty_wt(&self, terminal: &mut Term<EventProxy>) -> bool {
            true
        }

        fn launch_url_wt(&self, url: Url, terminal: &mut Term<EventProxy>) {

        }
        fn tab_manager(&self) -> Arc<RwLock<TabManager>> {
            return self.tab_manager.clone();
        }

        fn size_info(&self) -> SizeInfo {
            *self.size_info
        }

        fn selection_is_empty(&self) -> bool {
            true
        }

        fn scroll(&mut self, scroll: Scroll) {
            self.terminal.scroll_display(scroll);
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

        fn mouse_mode_wt(&self, terminal: &mut Term<EventProxy>) -> bool {
            false
        }
        fn mouse_mode(&self) -> bool {
            false
        }

        

        #[inline]
        fn mouse_mut(&mut self) -> &mut Mouse {
            self.mouse
        }

        #[inline]
        fn mouse(&self) -> &Mouse {
            self.mouse
        }

        fn received_count(&mut self) -> &mut usize {
            &mut self.received_count
        }

        fn suppress_chars(&mut self) -> &mut bool {
            &mut self.suppress_chars
        }

        fn modifiers(&mut self) -> &mut ModifiersState {
            &mut self.modifiers
        }

        fn window(&self) -> &Window {
            unimplemented!();
        }

        fn window_mut(&mut self) -> &mut Window {
            unimplemented!();
        }

        fn pop_message(&mut self) {
            self.message_buffer.pop();
        }

        fn message(&self) -> Option<&Message> {
            self.message_buffer.message()
        }

        fn config(&self) -> &Config {
            self.config
        }

        fn clipboard_mut(&mut self) -> &mut Clipboard {
            self.clipboard
        }

        fn event_loop(&self) -> &EventLoopWindowTarget<Event> {
            unimplemented!();
        }

        fn urls(&self) -> &Urls {
            unimplemented!();
        }

        fn launch_url(&self, _: Url) {
            unimplemented!();
        }

        fn scheduler_mut(&mut self) -> &mut Scheduler {
            unimplemented!();
        }

        fn on_typing_start(&mut self) {
            unimplemented!();
        }
    }

    macro_rules! test_clickstate {
        {
            name: $name:ident,
            initial_state: $initial_state:expr,
            initial_button: $initial_button:expr,
            input: $input:expr,
            end_state: $end_state:expr,
        } => {
            #[test]
            fn $name() {
                let mut clipboard = Clipboard::new_nop();
                let cfg = Config::default();
                let size = SizeInfo::new(
                    21.0,
                    51.0,
                    3.0,
                    3.0,
                    0.,
                    0.,
                    false,
                );

                let mut terminal = Term::new(&cfg, size, MockEventProxy);

                let mut mouse = Mouse {
                    click_state: $initial_state,
                    ..Mouse::default()
                };

                let mut selection = None;

                let mut message_buffer = MessageBuffer::new();

                let context = ActionContext {
                    terminal: &mut terminal,
                    selection: &mut selection,
                    mouse: &mut mouse,
                    size_info: &size,
                    clipboard: &mut clipboard,
                    received_count: 0,
                    suppress_chars: false,
                    modifiers: Default::default(),
                    message_buffer: &mut message_buffer,
                    config: &cfg,
                };

                let mut processor = Processor::new(context, &None);

                let event: GlutinEvent::<'_, TerminalEvent> = $input;
                if let GlutinEvent::WindowEvent {
                    event: WindowEvent::MouseInput {
                        state,
                        button,
                        ..
                    },
                    ..
                } = event
                {
                    processor.mouse_input(state, button);
                };

                assert_eq!(processor.ctx.mouse.click_state, $end_state);
            }
        }
    }

    macro_rules! test_process_binding {
        {
            name: $name:ident,
            binding: $binding:expr,
            triggers: $triggers:expr,
            mode: $mode:expr,
            mods: $mods:expr,
        } => {
            #[test]
            fn $name() {
                if $triggers {
                    assert!($binding.is_triggered_by($mode, $mods, &KEY));
                } else {
                    assert!(!$binding.is_triggered_by($mode, $mods, &KEY));
                }
            }
        }
    }

    test_clickstate! {
        name: single_click,
        initial_state: ClickState::None,
        initial_button: MouseButton::Other(0),
        input: GlutinEvent::WindowEvent {
            event: WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                device_id: unsafe { std::mem::transmute_copy(&0) },
                modifiers: ModifiersState::default(),
            },
            window_id: unsafe { std::mem::transmute_copy(&0) },
        },
        end_state: ClickState::Click,
    }

    test_clickstate! {
        name: single_right_click,
        initial_state: ClickState::None,
        initial_button: MouseButton::Other(0),
        input: GlutinEvent::WindowEvent {
            event: WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Right,
                device_id: unsafe { std::mem::transmute_copy(&0) },
                modifiers: ModifiersState::default(),
            },
            window_id: unsafe { std::mem::transmute_copy(&0) },
        },
        end_state: ClickState::Click,
    }

    test_clickstate! {
        name: single_middle_click,
        initial_state: ClickState::None,
        initial_button: MouseButton::Other(0),
        input: GlutinEvent::WindowEvent {
            event: WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Middle,
                device_id: unsafe { std::mem::transmute_copy(&0) },
                modifiers: ModifiersState::default(),
            },
            window_id: unsafe { std::mem::transmute_copy(&0) },
        },
        end_state: ClickState::None,
    }

    test_clickstate! {
        name: double_click,
        initial_state: ClickState::Click,
        initial_button: MouseButton::Left,
        input: GlutinEvent::WindowEvent {
            event: WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                device_id: unsafe { std::mem::transmute_copy(&0) },
                modifiers: ModifiersState::default(),
            },
            window_id: unsafe { std::mem::transmute_copy(&0) },
        },
        end_state: ClickState::DoubleClick,
    }

    test_clickstate! {
        name: triple_click,
        initial_state: ClickState::DoubleClick,
        initial_button: MouseButton::Left,
        input: GlutinEvent::WindowEvent {
            event: WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                device_id: unsafe { std::mem::transmute_copy(&0) },
                modifiers: ModifiersState::default(),
            },
            window_id: unsafe { std::mem::transmute_copy(&0) },
        },
        end_state: ClickState::TripleClick,
    }

    test_clickstate! {
        name: multi_click_separate_buttons,
        initial_state: ClickState::DoubleClick,
        initial_button: MouseButton::Left,
        input: GlutinEvent::WindowEvent {
            event: WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Right,
                device_id: unsafe { std::mem::transmute_copy(&0) },
                modifiers: ModifiersState::default(),
            },
            window_id: unsafe { std::mem::transmute_copy(&0) },
        },
        end_state: ClickState::Click,
    }

    test_process_binding! {
        name: process_binding_nomode_shiftmod_require_shift,
        binding: Binding { trigger: KEY, mods: ModifiersState::SHIFT, action: Action::from("\x1b[1;2D"), mode: BindingMode::empty(), notmode: BindingMode::empty() },
        triggers: true,
        mode: BindingMode::empty(),
        mods: ModifiersState::SHIFT,
    }

    test_process_binding! {
        name: process_binding_nomode_nomod_require_shift,
        binding: Binding { trigger: KEY, mods: ModifiersState::SHIFT, action: Action::from("\x1b[1;2D"), mode: BindingMode::empty(), notmode: BindingMode::empty() },
        triggers: false,
        mode: BindingMode::empty(),
        mods: ModifiersState::empty(),
    }

    test_process_binding! {
        name: process_binding_nomode_controlmod,
        binding: Binding { trigger: KEY, mods: ModifiersState::CTRL, action: Action::from("\x1b[1;5D"), mode: BindingMode::empty(), notmode: BindingMode::empty() },
        triggers: true,
        mode: BindingMode::empty(),
        mods: ModifiersState::CTRL,
    }

    test_process_binding! {
        name: process_binding_nomode_nomod_require_not_appcursor,
        binding: Binding { trigger: KEY, mods: ModifiersState::empty(), action: Action::from("\x1b[D"), mode: BindingMode::empty(), notmode: BindingMode::APP_CURSOR },
        triggers: true,
        mode: BindingMode::empty(),
        mods: ModifiersState::empty(),
    }

    test_process_binding! {
        name: process_binding_appcursormode_nomod_require_appcursor,
        binding: Binding { trigger: KEY, mods: ModifiersState::empty(), action: Action::from("\x1bOD"), mode: BindingMode::APP_CURSOR, notmode: BindingMode::empty() },
        triggers: true,
        mode: BindingMode::APP_CURSOR,
        mods: ModifiersState::empty(),
    }

    test_process_binding! {
        name: process_binding_nomode_nomod_require_appcursor,
        binding: Binding { trigger: KEY, mods: ModifiersState::empty(), action: Action::from("\x1bOD"), mode: BindingMode::APP_CURSOR, notmode: BindingMode::empty() },
        triggers: false,
        mode: BindingMode::empty(),
        mods: ModifiersState::empty(),
    }

    test_process_binding! {
        name: process_binding_appcursormode_appkeypadmode_nomod_require_appcursor,
        binding: Binding { trigger: KEY, mods: ModifiersState::empty(), action: Action::from("\x1bOD"), mode: BindingMode::APP_CURSOR, notmode: BindingMode::empty() },
        triggers: true,
        mode: BindingMode::APP_CURSOR | BindingMode::APP_KEYPAD,
        mods: ModifiersState::empty(),
    }

    test_process_binding! {
        name: process_binding_fail_with_extra_mods,
        binding: Binding { trigger: KEY, mods: ModifiersState::LOGO, action: Action::from("arst"), mode: BindingMode::empty(), notmode: BindingMode::empty() },
        triggers: false,
        mode: BindingMode::empty(),
        mods: ModifiersState::ALT | ModifiersState::LOGO,
    }
}
