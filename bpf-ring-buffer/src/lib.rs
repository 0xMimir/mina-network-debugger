use std::{
    fmt, io, mem,
    os::unix::io::AsRawFd,
    ptr, slice,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use smallvec::SmallVec;

pub trait RingBufferData
where
    Self: Sized,
{
    type Error: fmt::Debug;

    fn from_rb_slice(slice: &[u8]) -> Result<Option<Self>, Self::Error>;
}

pub struct RingBuffer {
    fd: i32,
    mask: usize,
    consumer_pos_value: usize,
    last_reported_percent: usize,
    // pointers to shared memory
    observer: RingBufferObserver,
}

impl AsRawFd for RingBuffer {
    fn as_raw_fd(&self) -> i32 {
        self.fd
    }
}

struct RingBufferObserver {
    page_size: usize,
    data: Box<[AtomicUsize]>,
    consumer_pos: Box<AtomicUsize>,
    producer_pos: Box<AtomicUsize>,
}

impl RingBufferObserver {
    #[allow(clippy::len_without_is_empty)]
    fn len(&self) -> usize {
        self.data.len() * mem::size_of::<AtomicUsize>()
    }
}

impl AsRef<[u8]> for RingBufferObserver {
    fn as_ref(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.data.as_ptr() as *const u8, self.len()) }
    }
}

impl RingBuffer {
    pub fn new(fd: i32, max_length: usize) -> io::Result<Self> {
        debug_assert_eq!(max_length & (max_length - 1), 0);

        // it is a constant, most likely 0x1000
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;

        // consumers page, currently contains only one integer value,
        // offset where consumer should read;
        // map it read/write
        let consumer_pos = unsafe {
            let p = libc::mmap(
                ptr::null_mut(),
                page_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if p == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }

            Box::from_raw(p as *mut AtomicUsize)
        };

        // producers page and the buffer itself,
        // currently producers page contains only one integer value,
        // offset where producer has wrote, or still writing;
        // let's refer buffer data as a slice of `AtomicUsize` array
        // because we care only about data headers which sized and aligned by 8;
        // map it read only
        let (producer_pos, data) = unsafe {
            let p = libc::mmap(
                ptr::null_mut(),
                page_size + max_length * 2,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                page_size as i64,
            );
            if p == libc::MAP_FAILED {
                libc::munmap(Box::into_raw(consumer_pos) as *mut _, page_size);
                return Err(io::Error::last_os_error());
            }

            let length = max_length * 2 / mem::size_of::<AtomicUsize>();
            let q = (p as usize) + page_size;
            let q = slice::from_raw_parts_mut(q as *mut AtomicUsize, length);
            (
                Box::from_raw(p as *mut AtomicUsize),
                Box::from_raw(q as *mut [AtomicUsize]),
            )
        };

        log::info!(
            "new RingBuffer: fd: {}, page_size: 0x{:016x}, mask: 0x{:016x}",
            fd,
            page_size,
            max_length - 1
        );
        Ok(RingBuffer {
            fd,
            mask: max_length - 1,
            consumer_pos_value: 0,
            last_reported_percent: 0,
            observer: RingBufferObserver {
                page_size,
                data,
                consumer_pos,
                producer_pos,
            },
        })
    }

    // try to read a data slice from the ring buffer, advance our position
    #[allow(clippy::comparison_chain)]
    fn read<D>(&mut self) -> io::Result<SmallVec<[D; 64]>>
    where
        D: RingBufferData,
    {
        const BUSY_BIT: usize = 1 << 31;
        const DISCARD_BIT: usize = 1 << 30;
        const HEADER_SIZE: usize = 8;
        const TOTAL_READ_THRESHOLD: usize = 0x100000; // 1MiB

        let mut vec = SmallVec::new();
        let mut read_total = 0;

        // try read something
        loop {
            let pr_pos = self.observer.producer_pos.load(Ordering::Acquire);
            if self.consumer_pos_value > pr_pos {
                // it means we were read a slice of memory which wasn't written yet
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "read uninitialized data",
                ));
            } else if self.consumer_pos_value == pr_pos {
                // nothing to read more
                // tell the kernel were we are
                break;
            } else {
                // determine how far we are, how many unseen data is in the buffer
                let distance = pr_pos - self.consumer_pos_value;
                let quant = (self.mask + 1) / 100;
                let percent = distance / quant;
                if percent >= 100 {
                    log::error!("the buffer is overflow");
                    // TODO:
                    std::process::exit(1);
                }
                if percent > self.last_reported_percent {
                    log::warn!("the buffer is filled by: {}%, increasing", percent);
                    self.last_reported_percent = percent;
                } else if percent < self.last_reported_percent {
                    log::info!("the buffer is filled by: {}%, decreasing", percent);
                    self.last_reported_percent = percent;
                }
            }

            // the first 8 bytes of the memory slice is a header (length and flags)
            let (header, data_offset) = {
                let masked_pos = self.consumer_pos_value & self.mask;
                let index_in_array = masked_pos / mem::size_of::<AtomicUsize>();
                let header = self.observer.data[index_in_array].load(Ordering::Acquire);
                // keep only 32 bits
                (header & 0xffffffff, masked_pos + HEADER_SIZE)
            };

            if header & BUSY_BIT != 0 {
                // nothing to read, kernel is writing to this slice right now
                // tell the kernel were we are
                break;
            }

            let (length, discard) = (header & !DISCARD_BIT, (header & DISCARD_BIT) != 0);

            // align the length by 8, and advance our position
            self.consumer_pos_value += HEADER_SIZE + (length + 7) / 8 * 8;

            if !discard {
                // if not discard, yield the slice
                let s = unsafe {
                    slice::from_raw_parts(
                        ((self.observer.data.as_ptr() as usize) + data_offset) as *mut u8,
                        length,
                    )
                };
                match D::from_rb_slice(s) {
                    Ok(None) => {
                        read_total += s.len();
                    }
                    Ok(Some(data)) => {
                        vec.push(data);
                        read_total += s.len();
                    }
                    Err(error) => log::error!("rb parse data: {:?}", error),
                }
            }
            // if kernel decide to discard this slice, go to the next iteration

            // store our position to tell kernel it can overwrite memory behind our position
            self.observer
                .consumer_pos
                .store(self.consumer_pos_value, Ordering::Release);

            if read_total > TOTAL_READ_THRESHOLD {
                break;
            }
        }

        if vec.is_empty() {
            Err(io::Error::new(io::ErrorKind::WouldBlock, ""))
        } else {
            Ok(vec)
        }
    }

    fn wait(&self, terminating: &AtomicBool) {
        let mut fds = libc::pollfd {
            fd: self.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        while !terminating.load(Ordering::Relaxed) {
            match unsafe { libc::poll(&mut fds, 1, 1_000) } {
                0 => log::debug!("ringbuf wait timeout"),
                1 => {
                    if fds.revents & libc::POLLIN != 0 {
                        break;
                    }
                }
                i32::MIN..=-1 => {
                    let error = io::Error::last_os_error();
                    if io::ErrorKind::Interrupted != error.kind() {
                        log::error!("ringbuf error: {:?}", error);
                    }
                }
                // poll should not return bigger then number of fds, we have 1
                r @ 2..=i32::MAX => log::error!("ringbuf poll {}", r),
            }
            fds.revents = 0;
        }
    }

    pub fn read_blocking<D>(&mut self, terminating: &AtomicBool) -> io::Result<SmallVec<[D; 64]>>
    where
        D: RingBufferData,
    {
        let mut tries = 0;
        loop {
            if tries > 10 {
                log::debug!("cannot read ring buffer: {} attempts", tries);
            }
            match self.read() {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.wait(terminating);
                    if terminating.load(Ordering::Relaxed) {
                        break Ok(SmallVec::new());
                    }
                }
                x => break x,
            }
            tries += 1;
        }
    }
}

impl Drop for RingBufferObserver {
    fn drop(&mut self) {
        let len = self.len();
        let p = mem::replace(&mut self.consumer_pos, Box::new(AtomicUsize::new(0)));
        let q = mem::replace(&mut self.producer_pos, Box::new(AtomicUsize::new(0)));
        let data = mem::replace(&mut self.data, Box::new([]));
        unsafe {
            libc::munmap(Box::into_raw(p) as *mut _, self.page_size);
            libc::munmap(Box::into_raw(q) as *mut _, self.page_size + len);
        }
        Box::leak(data);
    }
}
