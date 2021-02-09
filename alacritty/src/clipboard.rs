#[cfg(all(feature = "wayland", not(any(target_os = "macos", windows))))]
use std::ffi::c_void;

use log::{debug, warn};

use alacritty_terminal::term::ClipboardType;

use clipboard::ClipboardProvider;
use clipboard::ClipboardContext;

pub struct Clipboard {
}

impl Clipboard {
    pub fn new() -> Self {
        return Self {
        };
    }

    pub fn new_nop() -> Self {
        Self { }
        // TODO: Use clipboard::nop_clipboard::NopClipboardContext
    }
}


impl Clipboard {
    pub fn store(&mut self, ty: ClipboardType, text: impl Into<String>) {
        let mut ctx: clipboard::ClipboardContext = clipboard::ClipboardProvider::new().unwrap();
        ctx.set_contents(text.into().to_owned()).unwrap_or_else(|err| {
            warn!("Unable to store text in clipboard: {}", err);
        });
    }

    pub fn load(&mut self, ty: ClipboardType) -> String {
        let mut ctx: clipboard::ClipboardContext = clipboard::ClipboardProvider::new().unwrap();
        match ctx.get_contents() {
            Err(err) => {
                debug!("Unable to load text from clipboard: {}", err);
                String::new()
            },
            Ok(text) => text,
        }
    }
}
