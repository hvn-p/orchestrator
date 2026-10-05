//! Memory pressure, as the kernel reports it. A PSI trigger written to
//! `/proc/pressure/memory` makes the file readable as priority data
//! (`POLLPRI`) whenever tasks stall on memory for longer than a threshold
//! within a window. An unprivileged process gets triggers whose window is 2 s
//! or a multiple of it.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::path::Path;
use std::time::Duration;

/// The shortest window an unprivileged trigger may use.
pub const WINDOW: Duration = Duration::from_secs(2);

/// An armed trigger; dropping it disarms it.
pub struct Trigger {
    file: File,
}

impl Trigger {
    /// Fires when some task stalls on memory for `stall` within `WINDOW`.
    pub fn new(pressure_file: &Path, stall: Duration) -> io::Result<Trigger> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(pressure_file)?;
        // The kernel takes the whole trigger in a single write.
        file.write_all(spec(stall).as_bytes())?;
        Ok(Trigger { file })
    }
}

impl AsFd for Trigger {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}

fn spec(stall: Duration) -> String {
    format!("some {} {}\0", stall.as_micros(), WINDOW.as_micros())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_spec_is_in_microseconds() {
        assert_eq!(spec(Duration::from_millis(200)), "some 200000 2000000\0");
    }
}
