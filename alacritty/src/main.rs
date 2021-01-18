//! Alacritty - The GPU Enhanced Terminal.

#![warn(rust_2018_idioms, future_incompatible)]
#![deny(clippy::all, clippy::if_not_else, clippy::enum_glob_use, clippy::wrong_pub_self_convention)]
#![cfg_attr(feature = "cargo-clippy", deny(warnings))]
#![cfg_attr(all(test, feature = "bench"), feature(test))]
// With the default subsystem, 'console', windows creates an additional console
// window for the program.
// This is silently ignored on non-windows systems.
// See https://msdn.microsoft.com/en-us/library/4cc7ya5b.aspx for more details.
#![windows_subsystem = "windows"]

#[cfg(not(any(feature = "x11", feature = "wayland", target_os = "macos", windows)))]
compile_error!(r#"at least one of the "x11"/"wayland" features must be enabled"#);

use mio::event::Iter;
#[cfg(target_os = "macos")]
use std::env;
use std::error::Error;
use std::fs;
use std::io::{self, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mio::unix::EventedFd;
use mio::unix::UnixReady;
use mio::{self, Events, Poll, PollOpt, Ready, Token};
use mio_extras::channel::{channel, Receiver, Sender};
use std::os::unix::io::AsRawFd;

use glutin::event_loop::EventLoop as GlutinEventLoop;
use log::{error, info};
#[cfg(windows)]
use winapi::um::wincon::{AttachConsole, FreeConsole, ATTACH_PARENT_PROCESS};

use alacritty_terminal::event_loop::{self, EventLoop, Msg};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::tty;

use crate::tab_manager::TabManager;

#[macro_use]
mod macros;

mod child_pty;
mod cli;
mod clipboard;
mod config;
mod cursor;
mod daemon;
mod display;
mod event;
mod input;
mod logging;
#[cfg(target_os = "macos")]
mod macos;
mod message_bar;
mod meter;
#[cfg(windows)]
mod panic;
mod passwd;
mod renderer;
mod scheduler;
mod tab_manager;
mod tty_spawn;
mod url;
mod window;

#[cfg(all(feature = "wayland", not(any(target_os = "macos", windows))))]
mod wayland_theme;

mod gl {
    #![allow(clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/gl_bindings.rs"));
}

use crate::cli::Options;
use crate::config::monitor;
use crate::config::Config;
use crate::display::Display;
use crate::event::{Event, EventProxy, Processor};
#[cfg(target_os = "macos")]
use crate::macos::locale;
use crate::message_bar::MessageBuffer;

fn main() {
    color_backtrace::install();

    #[cfg(windows)]
    panic::attach_handler();

    // When linked with the windows subsystem windows won't automatically attach
    // to the console of the parent process, so we do it explicitly. This fails
    // silently if the parent has no console.
    #[cfg(windows)]
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }

    // Load command line options.
    let options = Options::new();

    // Setup glutin event loop.
    let window_event_loop = GlutinEventLoop::<Event>::with_user_event();

    // Initialize the logger as soon as possible as to capture output from other subsystems.
    let log_file = logging::initialize(&options, window_event_loop.create_proxy())
        .expect("Unable to initialize logger");

    // Load configuration file.
    let config = config::load(&options);

    // Update the log level from config.
    log::set_max_level(config.ui_config.debug.log_level);

    // Switch to home directory.
    #[cfg(target_os = "macos")]
    env::set_current_dir(dirs::home_dir().unwrap()).unwrap();
    // Set locale.
    #[cfg(target_os = "macos")]
    locale::set_locale_environment();

    // Store if log file should be deleted before moving config.
    let persistent_logging = config.ui_config.debug.persistent_logging;

    // Run Alacritty.
    if let Err(err) = run(window_event_loop, config, options) {
        error!("Alacritty encountered an unrecoverable error:\n\n\t{}\n", err);
        std::process::exit(1);
    }

    // Clean up logfile.
    if let Some(log_file) = log_file {
        if !persistent_logging && fs::remove_file(&log_file).is_ok() {
            let _ = writeln!(io::stdout(), "Deleted log file at \"{}\"", log_file.display());
        }
    }
}

/// Run Alacritty.
///
/// Creates a window, the terminal state, PTY, I/O event loop, input processor,
/// config change monitor, and runs the main display loop.
fn run(
    window_event_loop: GlutinEventLoop<Event>,
    config: Config,
    options: Options,
) -> Result<(), Box<dyn Error>> {
    info!("Welcome to Alacritty");

    // Log the configuration paths.
    log_config_path(&config);

    // Set environment variables.
    tty::setup_env(&config);

    let event_proxy = EventProxy::new(window_event_loop.create_proxy());

    let tab_manager = TabManager::new(event_proxy.clone(), config.clone());

    let tab_manager_mutex = Arc::new(FairMutex::new(tab_manager));

    // Create a display.
    //
    let tab_manager_mutex_display_clone = tab_manager_mutex.clone();
    let display = Display::new(&config, &window_event_loop, tab_manager_mutex_display_clone)?;
    info!(
        "PTY dimensions: {:?} x {:?}",
        display.size_info.screen_lines(),
        display.size_info.cols()
    );

    let tab_manager_mutex_main_clone = tab_manager_mutex.clone();
    let mut tab_manager_main_guard = tab_manager_mutex_main_clone.lock();
    let tab_manager = &mut *tab_manager_main_guard;
    tab_manager.set_size(display.size_info.clone());

    let idx = tab_manager.new_tab().unwrap();
    tab_manager.select_tab(idx);

    drop(tab_manager_main_guard);

    let event_proxy_clone = event_proxy.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(100));

        event_proxy_clone.send_event(crate::event::Event::TerminalEvent(
            alacritty_terminal::event::Event::Wakeup,
        ));
    });

    // Create a config monitor when config was loaded from path.
    //
    // The monitor watches the config file for changes and reloads it. Pending
    // config changes are processed in the main loop.
    if config.ui_config.live_config_reload {
        monitor::watch(config.ui_config.config_paths.clone(), event_proxy);
    }

    // Setup storage for message UI.
    let message_buffer = MessageBuffer::new();

    // Event processor.
    let tab_manager_processor_mutex_clone = tab_manager_mutex.clone();
    let mut processor =
        Processor::new(tab_manager_processor_mutex_clone, message_buffer, config, display, options);

    info!("Initialisation complete");

    println!("Starting input processor");

    // Start event loop and block until shutdown.
    processor.run(window_event_loop);

    // This explicit drop is needed for Windows, ConPTY backend. Otherwise a deadlock can occur.
    // The cause:
    //   - Drop for ConPTY will deadlock if the conout pipe has already been dropped.
    //   - The conout pipe is dropped when the io_thread is joined below (io_thread owns PTY).
    //   - ConPTY is dropped when the last of processor and io_thread are dropped, because both of
    //     them own an Arc<ConPTY>.
    //
    // The fix is to ensure that processor is dropped first. That way, when io_thread (i.e. PTY)
    // is dropped, it can ensure ConPTY is dropped before the conout pipe in the PTY drop order.
    //
    // FIXME: Change PTY API to enforce the correct drop order with the typesystem.
    drop(processor);

    // Shutdown PTY parser event loop.
    // loop_tx.send(Msg::Shutdown).expect("Error sending shutdown to PTY event loop");
    // io_thread.join().expect("join io thread");

    // FIXME patch notify library to have a shutdown method.
    // config_reloader.join().ok();

    // Without explicitly detaching the console cmd won't redraw it's prompt.
    #[cfg(windows)]
    unsafe {
        FreeConsole();
    }

    info!("Goodbye");

    Ok(())
}

fn log_config_path(config: &Config) {
    let mut msg = String::from("Configuration files loaded from:");
    for path in &config.ui_config.config_paths {
        msg.push_str(&format!("\n  {:?}", path.display()));
    }

    info!("{}", msg);
}
