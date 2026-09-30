use input::{Event, Libinput, LibinputInterface};
use rustix::fs::{open, Mode, OFlags};
use std::os::fd::OwnedFd;
use std::path::Path;

pub struct NixInterface;

impl LibinputInterface for NixInterface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        open(
            path,
            OFlags::from_bits_truncate(flags as u32),
            Mode::empty(),
        )
        .map_err(|err| err.raw_os_error())
    }
    fn close_restricted(&mut self, fd: OwnedFd) {
        let _ = fd;
    }
}

pub trait SyncEvent {
    fn wait_next_event(&mut self) -> Option<Event>;
}

impl SyncEvent for Libinput {
    fn wait_next_event(&mut self) -> Option<Event> {
        for _ in 0..10 {
            // loop maximum 10 times to avoid infinite loop
            self.dispatch().unwrap();
            if let Some(event) = self.next() {
                return Some(event);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        None
    }
}
