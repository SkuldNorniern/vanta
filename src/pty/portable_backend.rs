//! Cross-platform PTY backend via the `portable-pty` crate (ConPTY / forkpty).

use super::Pty;
use std::io::{self, Read, Write};
use std::sync::Mutex;

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

fn oerr<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(e.to_string())
}

/// Spawn the platform default shell on a portable-pty PTY.
pub fn spawn() -> io::Result<Box<dyn Pty>> {
    let system = native_pty_system();
    let pair = system
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(oerr)?;

    let mut cmd = CommandBuilder::new_default_prog();
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }
    // GUI processes don't inherit a terminal, so TERM is unset (or "dumb")
    // which suppresses color output from the shell and CLI tools. Advertise
    // ourselves as a 256-color xterm; COLORTERM lets truecolor-aware tools
    // know 24-bit RGB is also available.
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    let child = pair.slave.spawn_command(cmd).map_err(oerr)?;
    drop(pair.slave); // close the slave handle in this process

    let reader = pair.master.try_clone_reader().map_err(oerr)?;
    let writer = pair.master.take_writer().map_err(oerr)?;

    Ok(Box::new(PortablePty {
        master: Mutex::new(pair.master),
        writer: Mutex::new(writer),
        reader: Mutex::new(Some(reader)),
        child: Mutex::new(child),
    }))
}

struct PortablePty {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    reader: Mutex<Option<Box<dyn Read + Send>>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
}

impl Pty for PortablePty {
    fn take_reader(&mut self) -> Option<Box<dyn Read + Send>> {
        self.reader.lock().ok()?.take()
    }

    fn write(&self, data: &[u8]) -> io::Result<()> {
        let mut w = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("pty writer poisoned"))?;
        w.write_all(data)?;
        w.flush()
    }

    fn resize(&self, cols: u16, rows: u16) {
        if let Ok(master) = self.master.lock() {
            let _ = master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }

    fn is_running(&self) -> bool {
        self.child
            .lock()
            .map(|mut c| matches!(c.try_wait(), Ok(None)))
            .unwrap_or(false)
    }
}

impl Drop for PortablePty {
    fn drop(&mut self) {
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
        }
    }
}
