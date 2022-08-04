#![cfg_attr(
    feature = "kern",
    no_std,
    no_main,
    feature(lang_items),
    feature(core_ffi_c)
)]

#[cfg(feature = "kern")]
use ebpf_kern as ebpf;
#[cfg(feature = "user")]
use ebpf_user as ebpf;

#[cfg(feature = "kern")]
ebpf::license!("GPL");

#[cfg(any(feature = "kern", feature = "user"))]
#[derive(ebpf::BpfApp)]
pub struct App {
    // output channel
    #[ringbuf(size = 0x20000000)]
    pub event_queue: ebpf::RingBufferRef,
    // track relevant pids
    // 0x1000 processes maximum
    #[hashmap(size = 0x1000)]
    pub pid: ebpf::HashMapRef<4, 4>,
    #[prog("tracepoint/syscalls/sys_enter_execve")]
    pub execve: ebpf::ProgRef,
    #[prog("tracepoint/syscalls/sys_enter_execveat")]
    pub execveat: ebpf::ProgRef,
    // store/load context parameters
    // 0x100 cpus maximum
    #[hashmap(size = 0x100)]
    pub context_parameters: ebpf::HashMapRef<4, 0x20>,
    #[prog("tracepoint/syscalls/sys_enter_bind")]
    pub enter_bind: ebpf::ProgRef,
    #[prog("tracepoint/syscalls/sys_exit_bind")]
    pub exit_bind: ebpf::ProgRef,
    #[prog("tracepoint/syscalls/sys_enter_connect")]
    pub enter_connect: ebpf::ProgRef,
    #[prog("tracepoint/syscalls/sys_exit_connect")]
    pub exit_connect: ebpf::ProgRef,
    #[prog("tracepoint/syscalls/sys_enter_accept4")]
    pub enter_accept4: ebpf::ProgRef,
    #[prog("tracepoint/syscalls/sys_exit_accept4")]
    pub exit_accept4: ebpf::ProgRef,
    #[prog("tracepoint/syscalls/sys_enter_close")]
    pub enter_close: ebpf::ProgRef,
    // 0x4000 simultaneous connections maximum
    #[hashmap(size = 0x4000)]
    pub connections: ebpf::HashMapRef<8, 4>,
    #[prog("tracepoint/syscalls/sys_enter_write")]
    pub enter_write: ebpf::ProgRef,
    #[prog("tracepoint/syscalls/sys_exit_write")]
    pub exit_write: ebpf::ProgRef,
    #[prog("tracepoint/syscalls/sys_enter_read")]
    pub enter_read: ebpf::ProgRef,
    #[prog("tracepoint/syscalls/sys_exit_read")]
    pub exit_read: ebpf::ProgRef,
    #[prog("tracepoint/syscalls/sys_enter_sendto")]
    pub enter_sendto: ebpf::ProgRef,
    #[prog("tracepoint/syscalls/sys_exit_sendto")]
    pub exit_sendto: ebpf::ProgRef,
    #[prog("tracepoint/syscalls/sys_enter_recvfrom")]
    pub enter_recvfrom: ebpf::ProgRef,
    #[prog("tracepoint/syscalls/sys_exit_recvfrom")]
    pub exit_recvfrom: ebpf::ProgRef,
    #[prog("tracepoint/syscalls/sys_enter_getrandom")]
    pub enter_getrandom: ebpf::ProgRef,
    #[prog("tracepoint/syscalls/sys_exit_getrandom")]
    pub exit_getrandom: ebpf::ProgRef,
}

#[cfg(feature = "kern")]
mod context;

#[cfg(feature = "kern")]
mod send;

#[cfg(feature = "kern")]
use bpf_recorder::{DataTag, Event};

#[cfg(feature = "kern")]
impl App {
    #[inline(always)]
    fn check_pid(&self) -> Result<(), i32> {
        use ebpf::helpers;

        let x = unsafe { helpers::get_current_pid_tgid() };
        let pid = (x >> 32) as u32;

        if let Some(&flags) = self.pid.get(&pid.to_ne_bytes()) {
            let flags = u32::from_ne_bytes(flags);

            if flags == 0xffffffff {
                return Ok(());
            }
        }

        Err(0)
    }

    #[allow(clippy::nonminimal_bool)]
    #[inline(never)]
    fn check_env_entry(&mut self, entry: *const u8) -> Result<u32, i32> {
        use ebpf::helpers;

        let mut str_bytes = self.event_queue.reserve(0x200)?;
        let c = unsafe {
            helpers::probe_read_user_str(str_bytes.as_mut().as_mut_ptr() as _, 0x200, entry as _)
        };

        // Too short or too long
        if !(9..=0x200).contains(&c) {
            str_bytes.discard();
            return Err(c as _);
        }
        // Prefix is 'BPF_ALIAS'
        let prefix = true
            && str_bytes.as_ref()[0] == b'B'
            && str_bytes.as_ref()[1] == b'P'
            && str_bytes.as_ref()[2] == b'F'
            && str_bytes.as_ref()[3] == b'_'
            && str_bytes.as_ref()[4] == b'A'
            && str_bytes.as_ref()[5] == b'L'
            && str_bytes.as_ref()[6] == b'I'
            && str_bytes.as_ref()[7] == b'A'
            && str_bytes.as_ref()[8] == b'S';

        str_bytes.discard();
        if prefix {
            Ok((c - 10) as u32)
        } else {
            Err(0)
        }
    }

    fn check_env_flag(&mut self, env: *const *const u8) -> Result<(), i32> {
        use ebpf::helpers;

        if env.is_null() {
            return Err(0);
        }

        let mut i = 0;
        let mut env_str = self.event_queue.reserve(8)?;
        loop {
            let c = unsafe {
                helpers::probe_read_user(env_str.as_mut().as_mut_ptr() as _, 8, env.offset(i) as _)
            };

            if c != 0 {
                break;
            }
            i += 1;

            let entry = unsafe { *(env_str.as_ref().as_ptr() as *const *const u8) };
            if entry.is_null() {
                break;
            }

            if let Ok(len) = self.check_env_entry(entry) {
                env_str.discard();
                let x = unsafe { helpers::get_current_pid_tgid() };
                let pid = (x >> 32) as u32;

                let ts = unsafe { helpers::ktime_get_ns() };
                let event = Event::new(pid, ts, ts);
                let event = event.set_tag_fd(DataTag::Alias, 0).set_ok(len as u64);
                let name = unsafe { entry.offset(10) };
                send::dyn_sized::<typenum::B0>(&mut self.event_queue, event, name)?;

                return self
                    .pid
                    .insert(pid.to_ne_bytes(), 0x_ffff_ffff_u32.to_ne_bytes());
            }
            if i >= 0x100 {
                break;
            }
        }
        env_str.discard();

        Err(0)
    }

    #[inline(always)]
    pub fn execve(&mut self, ctx: ebpf::Context) -> Result<(), i32> {
        let env = ctx.read_here::<*const *const u8>(0x20);
        self.check_env_flag(env)
    }

    #[inline(always)]
    pub fn execveat(&mut self, ctx: ebpf::Context) -> Result<(), i32> {
        let env = ctx.read_here::<*const *const u8>(0x28);
        self.check_env_flag(env)
    }

    #[inline(always)]
    fn enter(&mut self, data: context::Variant) -> Result<(), i32> {
        use core::{mem, ptr};
        use ebpf::helpers;

        self.check_pid()?;
        let (_, thread_id) = {
            let x = unsafe { helpers::get_current_pid_tgid() };
            ((x >> 32) as u32, (x & 0xffffffff) as u32)
        };
        let ts = unsafe { helpers::ktime_get_ns() };

        let mut context = context::Parameters {
            data: context::Variant::Empty { ptr: 0, len: 0 },
            ts,
        };
        // bpf validator forbids reading from stack uninitialized data
        // different variants of this enum has different length,
        unsafe { ptr::write_volatile(&mut context.data, mem::zeroed()) };
        context.data = data;

        self.context_parameters
            .insert_unsafe(thread_id.to_ne_bytes(), context)
    }

    #[inline(always)]
    fn exit(&mut self, ctx: ebpf::Context) -> Result<(), i32> {
        use ebpf::helpers;

        self.check_pid()?;
        let (_, thread_id) = {
            let x = unsafe { helpers::get_current_pid_tgid() };
            ((x >> 32) as u32, (x & 0xffffffff) as u32)
        };
        let ts1 = unsafe { helpers::ktime_get_ns() };

        match self
            .context_parameters
            .remove_unsafe::<context::Parameters>(&thread_id.to_ne_bytes())?
        {
            Some(context::Parameters { data, ts: ts0 }) => {
                let ret = ctx.read_here(0x10);
                self.on_ret(ret, data, ts0, ts1)
            }
            None => Err(-1),
        }
    }

    #[inline(never)]
    fn on_ret(&mut self, ret: i64, data: context::Variant, ts0: u64, ts1: u64) -> Result<(), i32> {
        use ebpf::helpers;

        const EAGAIN: i64 = -11;
        if ret == EAGAIN {
            return Ok(());
        }

        fn check_addr(ptr: *const u8) -> Result<(), i32> {
            const AF_INET: u16 = 2;
            const AF_INET6: u16 = 10;

            let mut ty = 0u16;
            let c = unsafe { helpers::probe_read_user((&mut ty) as *mut _ as _, 2, ptr as _) };
            if c != 0 {
                // cannot read first two bytes of the address
                return Err(0);
            }
            if ty != AF_INET && ty != AF_INET6 {
                // ignore everything else
                return Err(0);
            }

            Ok(())
        }

        let x = unsafe { helpers::get_current_pid_tgid() };
        let pid = (x >> 32) as u32;

        let event = Event::new(pid, ts0, ts1);
        let ptr = data.ptr();
        let event = match data {
            context::Variant::Empty { len, .. } => {
                let event = event.set_tag_fd(DataTag::Debug, 0);
                if ret < 0 {
                    event.set_err(ret)
                } else {
                    event.set_ok(len)
                }
            }
            context::Variant::Bind { fd, addr_len, .. } => {
                let event = event.set_tag_fd(DataTag::Bind, fd);
                if ret < 0 {
                    event.set_err(ret)
                } else {
                    event.set_ok(addr_len)
                }
            }
            context::Variant::Connect { fd, addr_len, .. } => {
                check_addr(ptr)?;

                const EINPROGRESS: i64 = -115;
                let event = event.set_tag_fd(DataTag::Connect, fd);
                if ret < 0 && ret != EINPROGRESS {
                    event.set_err(ret)
                } else {
                    let socket_id = ((fd as u64) << 32) + (pid as u64);
                    self.connections
                        .insert(socket_id.to_ne_bytes(), 0x1_u32.to_ne_bytes())?;
                    event.set_ok(addr_len)
                }
            }
            context::Variant::Accept {
                listen_on_fd,
                addr_len,
                ..
            } => {
                let _ = listen_on_fd;
                let fd = ret as _;
                let event = event.set_tag_fd(DataTag::Accept, fd);
                if ret < 0 {
                    event.set_err(ret)
                } else {
                    check_addr(ptr)?;
                    let socket_id = ((fd as u64) << 32) + (pid as u64);
                    self.connections
                        .insert(socket_id.to_ne_bytes(), 0x1_u32.to_ne_bytes())?;
                    event.set_ok(addr_len)
                }
            }
            context::Variant::Send { fd, .. } | context::Variant::Write { fd, .. } => {
                let event = event.set_tag_fd(DataTag::Write, fd);
                let socket_id = ((fd as u64) << 32) + (pid as u64);
                if self.connections.get(&socket_id.to_ne_bytes()).is_none() {
                    return Ok(());
                }
                if ret < 0 {
                    event.set_err(ret)
                } else {
                    event.set_ok(ret as _)
                }
            }
            context::Variant::Recv { fd, .. } | context::Variant::Read { fd, .. } => {
                let event = event.set_tag_fd(DataTag::Read, fd);
                let socket_id = ((fd as u64) << 32) + (pid as u64);
                if self.connections.get(&socket_id.to_ne_bytes()).is_none() {
                    return Ok(());
                }
                if ret < 0 {
                    event.set_err(ret)
                } else {
                    event.set_ok(ret as _)
                }
            }
            context::Variant::GetRandom { data_len, .. } => {
                event.set_tag_fd(DataTag::Random, 0).set_ok(data_len)
            }
        };
        send::dyn_sized::<typenum::B0>(&mut self.event_queue, event, ptr)
    }

    #[inline(always)]
    pub fn enter_bind(&mut self, ctx: ebpf::Context) -> Result<(), i32> {
        self.enter(context::Variant::Bind {
            fd: ctx.read_here::<u64>(0x10) as u32,
            addr_ptr: ctx.read_here::<u64>(0x18),
            addr_len: ctx.read_here::<u64>(0x20),
        })
    }

    #[inline(always)]
    pub fn exit_bind(&mut self, ctx: ebpf::Context) -> Result<(), i32> {
        self.exit(ctx)
    }

    #[inline(always)]
    pub fn enter_connect(&mut self, ctx: ebpf::Context) -> Result<(), i32> {
        self.enter(context::Variant::Connect {
            fd: ctx.read_here::<u64>(0x10) as u32,
            addr_ptr: ctx.read_here::<u64>(0x18),
            addr_len: ctx.read_here::<u64>(0x20),
        })
    }

    #[inline(always)]
    pub fn exit_connect(&mut self, ctx: ebpf::Context) -> Result<(), i32> {
        self.exit(ctx)
    }

    #[inline(always)]
    pub fn enter_accept4(&mut self, ctx: ebpf::Context) -> Result<(), i32> {
        self.enter(context::Variant::Accept {
            listen_on_fd: ctx.read_here::<u64>(0x10) as u32,
            addr_ptr: ctx.read_here::<u64>(0x18),
            addr_len: ctx.read_here::<u64>(0x20),
        })
    }

    #[inline(always)]
    pub fn exit_accept4(&mut self, ctx: ebpf::Context) -> Result<(), i32> {
        self.exit(ctx)
    }

    #[inline(always)]
    pub fn enter_close(&mut self, ctx: ebpf::Context) -> Result<(), i32> {
        use core::ptr;
        use ebpf::helpers;

        self.check_pid()?;

        let fd = ctx.read_here::<u64>(0x10) as u32;
        let (pid, _) = {
            let x = unsafe { helpers::get_current_pid_tgid() };
            ((x >> 32) as u32, (x & 0xffffffff) as u32)
        };
        let ts = unsafe { helpers::ktime_get_ns() };

        let socket_id = ((fd as u64) << 32) + (pid as u64);
        if self.connections.remove(&socket_id.to_ne_bytes())?.is_none() {
            return Ok(());
        }

        let event = Event::new(pid, ts, ts);
        let event = event.set_tag_fd(DataTag::Close, fd);
        send::dyn_sized::<typenum::B0>(&mut self.event_queue, event, ptr::null())
    }

    #[inline(always)]
    pub fn enter_write(&mut self, ctx: ebpf::Context) -> Result<(), i32> {
        self.enter(context::Variant::Write {
            fd: ctx.read_here::<u64>(0x10) as u32,
            data_ptr: ctx.read_here::<u64>(0x18),
            _pad: 0,
        })
    }

    #[inline(always)]
    pub fn exit_write(&mut self, ctx: ebpf::Context) -> Result<(), i32> {
        self.exit(ctx)
    }

    #[inline(always)]
    pub fn enter_read(&mut self, ctx: ebpf::Context) -> Result<(), i32> {
        self.enter(context::Variant::Read {
            fd: ctx.read_here::<u64>(0x10) as u32,
            data_ptr: ctx.read_here::<u64>(0x18),
            _pad: 0,
        })
    }

    #[inline(always)]
    pub fn exit_read(&mut self, ctx: ebpf::Context) -> Result<(), i32> {
        self.exit(ctx)
    }

    #[inline(always)]
    pub fn enter_sendto(&mut self, ctx: ebpf::Context) -> Result<(), i32> {
        self.enter(context::Variant::Send {
            fd: ctx.read_here::<u64>(0x10) as u32,
            data_ptr: ctx.read_here::<u64>(0x18),
            _pad: 0,
        })
    }

    #[inline(always)]
    pub fn exit_sendto(&mut self, ctx: ebpf::Context) -> Result<(), i32> {
        self.exit(ctx)
    }

    #[inline(always)]
    pub fn enter_recvfrom(&mut self, ctx: ebpf::Context) -> Result<(), i32> {
        self.enter(context::Variant::Recv {
            fd: ctx.read_here::<u64>(0x10) as u32,
            data_ptr: ctx.read_here::<u64>(0x18),
            _pad: 0,
        })
    }

    #[inline(always)]
    pub fn exit_recvfrom(&mut self, ctx: ebpf::Context) -> Result<(), i32> {
        self.exit(ctx)
    }

    #[inline(always)]
    pub fn enter_getrandom(&mut self, ctx: ebpf::Context) -> Result<(), i32> {
        let len = ctx.read_here::<u64>(0x18);
        if len == 32 {
            self.enter(context::Variant::GetRandom {
                _fd: 0,
                data_ptr: ctx.read_here::<u64>(0x10),
                data_len: len,
            })
        } else {
            Ok(())
        }
    }

    #[inline(always)]
    pub fn exit_getrandom(&mut self, ctx: ebpf::Context) -> Result<(), i32> {
        self.exit(ctx)
    }
}

#[cfg(feature = "user")]
fn main() {
    use std::{
        collections::BTreeMap,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::{SystemTime, Duration},
    };

    use bpf_recorder::sniffer_event::{SnifferEvent, SnifferEventVariant};
    use bpf_ring_buffer::RingBuffer;
    use mina_recorder::{EventMetadata, ConnectionId};
    use ebpf::{kind::AppItem, Skeleton};

    let env = env_logger::Env::default().default_filter_or("info");
    env_logger::init_from_env(env);
    sudo::escalate_if_needed().expect("failed to obtain superuser permission");
    let terminating = Arc::new(AtomicBool::new(false));
    {
        let terminating = terminating.clone();
        ctrlc::set_handler(move || {
            terminating.store(true, Ordering::Relaxed);
        })
        .expect("Error setting Ctrl-C handler");
    }

    static CODE: &[u8] = include_bytes!(concat!("../", env!("BPF_CODE_RECORDER")));

    let mut skeleton = Skeleton::<App>::open("bpf-recorder\0", CODE)
        .unwrap_or_else(|code| panic!("failed to open bpf: {}", code));
    skeleton
        .load()
        .unwrap_or_else(|code| panic!("failed to load bpf: {}", code));
    let (skeleton, mut app) = skeleton
        .attach()
        .unwrap_or_else(|code| panic!("failed to attach bpf: {}", code));
    log::info!("attached bpf module");

    let fd = match app.event_queue.kind_mut() {
        ebpf::kind::AppItemKindMut::Map(map) => map.fd(),
        _ => unreachable!(),
    };

    let mut info = libbpf_sys::bpf_map_info::default();
    let mut len = std::mem::size_of::<libbpf_sys::bpf_map_info>() as u32;
    unsafe {
        libbpf_sys::bpf_obj_get_info_by_fd(
            fd,
            &mut info as *mut libbpf_sys::bpf_map_info as *mut _,
            &mut len as _,
        )
    };
    let mut rb = RingBuffer::new(fd, info.max_entries as usize).unwrap();

    const P2P_PORT: u16 = 8302;
    let mut apps = BTreeMap::new();
    let mut p2p_cns = BTreeMap::new();
    let mut ignored_cns = BTreeMap::new();
    let mut recorder = mina_recorder::P2pRecorder::default();
    let mut origin = None::<SystemTime>;
    let mut last_ts = 0;
    while !terminating.load(Ordering::Relaxed) {
        while let Ok(Some(event)) = rb.read_blocking::<SnifferEvent>(&terminating) {
            if event.ts0 + 1_000_000_000 < last_ts {
                log::error!("unordered {} < {last_ts}", event.ts0);
            }
            last_ts = event.ts0;
            let time = match &origin {
                None => {
                    let now = SystemTime::now();
                    origin = Some(now - Duration::from_nanos(event.ts0));
                    now
                }
                Some(origin) => *origin + Duration::from_nanos(event.ts0),
            };
            let duration = Duration::from_nanos(event.ts1 - event.ts0);
            match event.variant {
                SnifferEventVariant::NewApp(alias) => {
                    log::info!("exec {alias} pid: {}", event.pid);
                    apps.insert(event.pid, alias);
                }
                SnifferEventVariant::Error(_, _) => (),
                SnifferEventVariant::OutgoingConnection(addr) => {
                    if let Some(alias) = apps.get(&event.pid) {
                        if addr.port() == P2P_PORT {
                            let metadata = EventMetadata {
                                id: ConnectionId {
                                    alias: alias.clone(),
                                    addr,
                                    pid: event.pid,
                                    fd: event.fd,
                                },
                                time,
                                duration,
                            };
                            if let Some(old_addr) = p2p_cns.insert((event.pid, event.fd), addr) {
                                log::warn!("new outgoing connection on already allocated fd");
                                let mut metadata = metadata.clone();
                                metadata.id.addr = old_addr;
                                recorder.on_disconnect(metadata);
                            }
                            recorder.on_connect(false, metadata);
                        } else {
                            ignored_cns.insert((event.pid, event.fd), addr);
                        }
                    }
                }
                SnifferEventVariant::IncomingConnection(addr) => {
                    if let Some(alias) = apps.get(&event.pid) {
                        if addr.port() == P2P_PORT || addr.port() >= 49152 {
                            let metadata = EventMetadata {
                                id: ConnectionId {
                                    alias: alias.clone(),
                                    addr,
                                    pid: event.pid,
                                    fd: event.fd,
                                },
                                time,
                                duration,
                            };
                            if let Some(old_addr) = p2p_cns.insert((event.pid, event.fd), addr) {
                                log::warn!("new incoming connection on already allocated fd");
                                let mut metadata = metadata.clone();
                                metadata.id.addr = old_addr;
                                recorder.on_disconnect(metadata);
                            }
                            recorder.on_connect(true, metadata);
                        } else {
                            ignored_cns.insert((event.pid, event.fd), addr);
                        }
                    }
                }
                SnifferEventVariant::Disconnected => {
                    if let Some(alias) = apps.get(&event.pid) {
                        let key = (event.pid, event.fd);
                        if let Some(addr) = p2p_cns.remove(&key) {
                            let metadata = EventMetadata {
                                id: ConnectionId {
                                    alias: alias.clone(),
                                    addr,
                                    pid: event.pid,
                                    fd: event.fd,
                                },
                                time,
                                duration,
                            };
                            recorder.on_disconnect(metadata);
                        } else if !ignored_cns.contains_key(&key) {
                            log::debug!("{alias} cannot disconnect {fd}, not connected");
                        }
                    }
                }
                SnifferEventVariant::IncomingData(data) => {
                    if let Some(alias) = apps.get(&event.pid) {
                        let key = (event.pid, event.fd);
                        if let Some(addr) = p2p_cns.get(&key) {
                            let metadata = EventMetadata {
                                id: ConnectionId {
                                    alias: alias.clone(),
                                    addr: *addr,
                                    pid: event.pid,
                                    fd: event.fd,
                                },
                                time,
                                duration,
                            };
                            recorder.on_data(true, metadata, data);
                        } else if !ignored_cns.contains_key(&key) {
                            log::debug!(
                                "{alias} cannot handle data on {fd}, not connected, {}",
                                hex::encode(data),
                            );
                        }
                    }
                }
                SnifferEventVariant::OutgoingData(data) => {
                    if let Some(alias) = apps.get(&event.pid) {
                        let key = (event.pid, event.fd);
                        if let Some(addr) = p2p_cns.get(&key) {
                            let metadata = EventMetadata {
                                id: ConnectionId {
                                    alias: alias.clone(),
                                    addr: *addr,
                                    pid: event.pid,
                                    fd: event.fd,
                                },
                                time,
                                duration,
                            };
                            recorder.on_data(false, metadata, data);
                        } else if !ignored_cns.contains_key(&key) {
                            log::debug!(
                                "{alias} cannot handle data on {fd}, not connected, {}",
                                hex::encode(data),
                            );
                        }
                    }
                }
                SnifferEventVariant::Random(random) => {
                    if let Some(alias) = apps.get(&event.pid) {
                        recorder.on_randomness(alias.clone(), random);
                    }
                }
            }
        }
    }
    log::info!("terminated");
    drop(skeleton);
}
