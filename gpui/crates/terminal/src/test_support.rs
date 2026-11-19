//! Adapted from TRU-276 pty_counter.rs: observe the real event-loop reader.
use crate::{Backend, BackendSettings};
use alacritty_terminal::{
    event::{OnResize, WindowSize},
    tty::{self, ChildEvent, EventedPty, EventedReadWrite},
};
use polling::{Event, PollMode, Poller};
use std::{
    fs::File,
    io::{self, Read},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

pub fn python(script: String) -> Backend {
    Backend::new(BackendSettings {
        program: "/usr/bin/python3".into(),
        args: vec!["-u".into(), "-c".into(), script],
        ..Default::default()
    })
    .expect("spawn owned Python PTY")
}
pub fn wait(mut predicate: impl FnMut() -> bool) {
    let start = Instant::now();
    while !predicate() {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "PTY gate deadline"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}
pub fn query_script(query: &[u8], expected: &[u8]) -> String {
    format!(
        r#"import os, tty, select, time
tty.setraw(0)
os.write(1, bytes.fromhex('{query}'))
reply = b''
end = time.monotonic() + 3
while len(reply) < {size} and time.monotonic() < end:
    if select.select([0], [], [], max(0,end-time.monotonic()))[0]:
        reply += os.read(0,4096)
os.write(1, b'\x1b]2;' + (b'OK' if reply == bytes.fromhex('{expected}') else b'FAIL:' + reply.hex().encode()) + b'\x07')
"#,
        query = hex(query),
        expected = hex(expected),
        size = expected.len()
    )
}
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub const TOTAL: usize = 200_000_000;
pub const PATTERN: &[u8] = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\r\n";
pub const TAIL: &[u8] = b"\x1b[HFINAL_HIDDEN_MARKER\x1b[K\x1b]2;DONE\x07";
#[derive(Default)]
pub struct Counts {
    pub bytes: AtomicUsize,
    pub bad: AtomicUsize,
}
pub struct Reader {
    file: File,
    counts: Arc<Counts>,
    offset: usize,
}
impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.file.read(buf)?;
        self.counts
            .bad
            .fetch_add(mismatches(&buf[..n], self.offset), Ordering::Relaxed);
        self.offset += n;
        self.counts.bytes.store(self.offset, Ordering::Release);
        Ok(n)
    }
}
// Compare every byte in pattern-sized slices. The previous debug-mode loop
// spent the flood deadline doing a modulo and match per byte.
fn mismatches(mut bytes: &[u8], mut offset: usize) -> usize {
    let mut bad = 0;
    while !bytes.is_empty() {
        let expected = if offset < TOTAL {
            let pattern = &PATTERN[offset % PATTERN.len()..];
            &pattern[..pattern.len().min(TOTAL - offset)]
        } else {
            TAIL.get(offset - TOTAL..).unwrap_or_default()
        };
        if expected.is_empty() {
            return bad + bytes.len();
        }
        let n = bytes.len().min(expected.len());
        if bytes[..n] != expected[..n] {
            bad += bytes[..n]
                .iter()
                .zip(&expected[..n])
                .filter(|(a, b)| a != b)
                .count();
        }
        bytes = &bytes[n..];
        offset += n;
    }
    bad
}

#[test]
fn flood_observer_detects_each_bad_byte_across_pattern_and_tail_boundaries() {
    for start in [
        0,
        1,
        PATTERN.len() - 1,
        TOTAL - 7,
        TOTAL,
        TOTAL + TAIL.len(),
    ] {
        let expected: Vec<u8> = (start..start + 131)
            .map(|position| {
                if position < TOTAL {
                    PATTERN[position % PATTERN.len()]
                } else {
                    TAIL.get(position - TOTAL).copied().unwrap_or(0)
                }
            })
            .collect();
        for corrupt in [None, Some(0), Some(65), Some(130)] {
            let mut bytes = expected.clone();
            if let Some(i) = corrupt {
                bytes[i] ^= 1;
            }
            let scalar_bad = bytes
                .iter()
                .enumerate()
                .filter(|(i, byte)| {
                    let position = start + i;
                    let expected = if position < TOTAL {
                        Some(PATTERN[position % PATTERN.len()])
                    } else {
                        TAIL.get(position - TOTAL).copied()
                    };
                    expected != Some(**byte)
                })
                .count();
            assert_eq!(mismatches(&bytes, start), scalar_bad);
        }
    }
}

pub struct ObservedPty {
    pty: tty::Pty,
    reader: Reader,
}
impl ObservedPty {
    pub fn new(mut pty: tty::Pty, counts: Arc<Counts>) -> Self {
        let file = pty
            .reader()
            .try_clone()
            .expect("clone owned probe PTY reader");
        Self {
            pty,
            reader: Reader {
                file,
                counts,
                offset: 0,
            },
        }
    }
}
impl EventedReadWrite for ObservedPty {
    type Reader = Reader;
    type Writer = File;
    unsafe fn register(
        &mut self,
        poll: &Arc<Poller>,
        event: Event,
        mode: PollMode,
    ) -> io::Result<()> {
        // The owned PTY outlives registration, as required by Alacritty.
        unsafe { self.pty.register(poll, event, mode) }
    }
    fn reregister(&mut self, poll: &Arc<Poller>, event: Event, mode: PollMode) -> io::Result<()> {
        self.pty.reregister(poll, event, mode)
    }
    fn deregister(&mut self, poll: &Arc<Poller>) -> io::Result<()> {
        self.pty.deregister(poll)
    }
    fn reader(&mut self) -> &mut Reader {
        &mut self.reader
    }
    fn writer(&mut self) -> &mut File {
        self.pty.writer()
    }
}
impl EventedPty for ObservedPty {
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        self.pty.next_child_event()
    }
}
impl OnResize for ObservedPty {
    fn on_resize(&mut self, size: WindowSize) {
        self.pty.on_resize(size);
    }
}

pub fn rss_kib() -> u64 {
    let mut info = std::mem::MaybeUninit::<mach2::task_info::mach_task_basic_info>::zeroed();
    let mut count = mach2::task_info::MACH_TASK_BASIC_INFO_COUNT;
    // Only inspect this test process; no other process is signalled or enumerated.
    let result = unsafe {
        mach2::task::task_info(
            mach2::traps::mach_task_self(),
            mach2::task_info::MACH_TASK_BASIC_INFO,
            info.as_mut_ptr().cast(),
            &mut count,
        )
    };
    assert_eq!(result, mach2::kern_return::KERN_SUCCESS);
    unsafe { info.assume_init().resident_size / 1024 }
}
