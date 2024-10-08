use std::ffi::OsStr;
use std::io;
use std::iter::once;
use std::os::windows::ffi::OsStrExt;

use alacritty_terminal::config::{Config, Program};
use alacritty_terminal::term::SizeInfo;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;

use std::i16;
use std::io::Error;
use std::mem;
use std::os::windows::io::IntoRawHandle;
use std::ptr;

use winapi::shared::basetsd::{PSIZE_T, SIZE_T};
use winapi::shared::minwindef::BYTE;
use winapi::shared::ntdef::LPWSTR;
use winapi::shared::winerror::S_OK;
use winapi::um::consoleapi::{ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole};
use winapi::um::processthreadsapi::{
    CreateProcessW, InitializeProcThreadAttributeList, UpdateProcThreadAttribute,
    PROCESS_INFORMATION, STARTUPINFOW,
};
use winapi::um::winbase::{EXTENDED_STARTUPINFO_PRESENT, STARTF_USESTDHANDLES, STARTUPINFOEXW};
use winapi::um::wincontypes::{COORD, HPCON};

mod child;

pub use self::child::EVENT_LOOP_PROXY;

use self::child::ChildExitWatcher;

/// RAII Pseudoconsole.
pub struct Conpty {
    pub handle: HPCON,
}

impl Drop for Conpty {
    fn drop(&mut self) {
        // XXX: This will block until the conout pipe is drained. Will cause a deadlock if the
        // conout pipe has already been dropped by this point.
        //
        // See PR #3084 and https://docs.microsoft.com/en-us/windows/console/closepseudoconsole.
        unsafe { ClosePseudoConsole(self.handle) }
    }
}

impl Conpty {
    pub fn on_resize(&mut self, sizeinfo: &SizeInfo) {
        if let Some(coord) = coord_from_sizeinfo(sizeinfo) {
            let result = unsafe { ResizePseudoConsole(self.handle, coord) };
            assert_eq!(result, S_OK);
        }
    }
}

// The ConPTY handle can be sent between threads.
unsafe impl Send for Conpty {}

pub fn new(config: Config<crate::config::ui_config::UiConfig>, size: SizeInfo) -> Option<Pty> {
    let mut pty_handle = 0 as HPCON;

    // Passing 0 as the size parameter allows the "system default" buffer
    // size to be used. There may be small performance and memory advantages
    // to be gained by tuning this in the future, but it's likely a reasonable
    // start point.
    let (conout, conout_pty_handle) = miow::pipe::anonymous(0).unwrap();
    let (conin_pty_handle, conin) = miow::pipe::anonymous(0).unwrap();

    let coord =
        coord_from_sizeinfo(&size).expect("Overflow when creating initial size on pseudoconsole");

    // Create the Pseudo Console, using the pipes.
    let result = unsafe {
        CreatePseudoConsole(
            coord,
            conin_pty_handle.into_raw_handle(),
            conout_pty_handle.into_raw_handle(),
            0,
            &mut pty_handle as *mut HPCON,
        )
    };

    assert_eq!(result, S_OK);

    let mut success;

    // Prepare child process startup info.

    let mut size: SIZE_T = 0;

    let mut startup_info_ex: STARTUPINFOEXW = Default::default();

    startup_info_ex.StartupInfo.lpTitle = std::ptr::null_mut() as LPWSTR;

    startup_info_ex.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;

    // Setting this flag but leaving all the handles as default (null) ensures the
    // PTY process does not inherit any handles from this Alacritty process.
    startup_info_ex.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;

    // Create the appropriately sized thread attribute list.
    unsafe {
        let failure =
            InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut size as PSIZE_T) > 0;

        // This call was expected to return false.
        if failure {
            panic_shell_spawn();
        }
    }

    let mut attr_list: Box<[BYTE]> = vec![0; size].into_boxed_slice();

    // Set startup info's attribute list & initialize it
    //
    // Lint failure is spurious; it's because winapi's definition of PROC_THREAD_ATTRIBUTE_LIST
    // implies it is one pointer in size (32 or 64 bits) but really this is just a dummy value.
    // Casting a *mut u8 (pointer to 8 bit type) might therefore not be aligned correctly in
    // the compiler's eyes.
    #[allow(clippy::cast_ptr_alignment)]
    {
        startup_info_ex.lpAttributeList = attr_list.as_mut_ptr() as _;
    }

    unsafe {
        success = InitializeProcThreadAttributeList(
            startup_info_ex.lpAttributeList,
            1,
            0,
            &mut size as PSIZE_T,
        ) > 0;

        if !success {
            panic_shell_spawn();
        }
    }


    // Set thread attribute list's Pseudo Console to the specified ConPTY.
    unsafe {
        success = UpdateProcThreadAttribute(
            startup_info_ex.lpAttributeList,
            0,
            22 | 0x0002_0000, // PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE.
            pty_handle,
            mem::size_of::<HPCON>(),
            ptr::null_mut(),
            ptr::null_mut(),
        ) > 0;

        if !success {
            panic_shell_spawn();
        }
    }

    let cmdline = win32_string(&cmdline(&config));
    let cwd = config.working_directory.as_ref().map(win32_string);

    let mut proc_info: PROCESS_INFORMATION = Default::default();
    unsafe {
        success = CreateProcessW(
            ptr::null(),
            cmdline.as_ptr() as LPWSTR,
            ptr::null_mut(),
            ptr::null_mut(),
            false as i32,
            EXTENDED_STARTUPINFO_PRESENT,
            ptr::null_mut(),
            cwd.as_ref().map_or_else(ptr::null, |s| s.as_ptr()),
            &mut startup_info_ex.StartupInfo as *mut STARTUPINFOW,
            &mut proc_info as *mut PROCESS_INFORMATION,
        ) > 0;

        if !success {
            panic_shell_spawn();
        }
    }

    let _child_watcher = ChildExitWatcher::new(proc_info.hProcess).unwrap();
    let conpty = Conpty { handle: pty_handle };

    Some(Pty::new(conpty, 
                  conout, 
                  conin, 
                ).unwrap())
}

// Panic with the last os error as message.
fn panic_shell_spawn() {
    panic!("Unable to spawn shell: {}", Error::last_os_error());
}

/// Helper to build a COORD from a SizeInfo, returning None in overflow cases.
fn coord_from_sizeinfo(size: &SizeInfo) -> Option<COORD> {
    let cols = size.columns();
    let lines = size.screen_lines();

    if cols <= i16::MAX as usize && lines <= i16::MAX as usize {
        Some(COORD { X: cols as i16, Y: lines as i16 })
    } else {
        None
    }
}

pub struct Pty {
    pub backend: Conpty,
    pub fout: miow::pipe::AnonRead,
    pub fin: miow::pipe::AnonWrite,
}

impl io::Read for Pty {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let nbytes = self.fout.read(buf)?;
        Ok(nbytes)
    }
}

impl std::io::Write for Pty {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.fin.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::result::Result<(), std::io::Error> {
        self.fin.flush()?;
        Ok(())
    }
}

impl Pty {
    pub fn new(
        backend: impl Into<Conpty>,
        conout: miow::pipe::AnonRead,
        conin: miow::pipe::AnonWrite,
    ) -> Result<Self, ()> {
        Ok(Self {
            backend: backend.into(),
            fout: conout,
            fin: conin,
        })
    }

    pub fn fin_clone(&mut self) -> miow::pipe::AnonRead {
        unsafe { miow::pipe::AnonRead::from_raw_handle(self.fout.as_raw_handle()) }
    }

    pub fn on_resize(&mut self, size: &SizeInfo) {
        self.backend.on_resize(size)
    }
}

fn cmdline<C>(config: &Config<C>) -> String {
    let default_shell = Program::Just("powershell".to_owned());
    let shell = config.shell.as_ref().unwrap_or(&default_shell);

    once(shell.program().as_ref())
        .chain(shell.args().iter().map(|a| a.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Converts the string slice into a Windows-standard representation for "W"-
/// suffixed function variants, which accept UTF-16 encoded string values.
pub fn win32_string<S: AsRef<OsStr> + ?Sized>(value: &S) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(once(0)).collect()
}
