//! A virtio GPU, with virglrenderer doing the real work.
//!
//! Android draws with its virgl driver, which sends OpenGL commands through
//! these rings, and the renderer replays them on the host's GPU. This is API
//! forwarding, not passthrough. The device only unpacks requests, so it can
//! be tested with a fake [`Backend`]. Everything from the guest is untrusted.
//!
//! Requests finish before the device answers, because the vCPU is stopped
//! while it works. So a fence can be echoed at once: every later command
//! already runs after the one it guards, on the same OpenGL stream.

use std::collections::HashMap;

use rax_host::virgl::{Renderer, ResourceArgs, Transfer, TransferBox};

use crate::virtio_blk::GuestMemory;
use crate::virtio_input::{DEVICE_OFFSET, ISR_OFFSET, NOTIFY_OFFSET};
use crate::virtio_modern::{Chain, Common, RingError, VERSION_1, complete, next_chain};

const FEATURE_VIRGL: u64 = 1;
const QUEUE_SIZE: u16 = 256;
const CONTROL_QUEUE: usize = 0;
const QUEUES: usize = 2;
/// Longest request accepted. The largest real ones are backing page lists.
const MAX_REQUEST: usize = 16 * 1024 * 1024;
const HEADER_BYTES: usize = 24;
const FLAG_FENCE: u32 = 1;
const SCANOUTS: usize = 16;

const CMD_GET_DISPLAY_INFO: u32 = 0x100;
const CMD_RESOURCE_CREATE_2D: u32 = 0x101;
const CMD_RESOURCE_UNREF: u32 = 0x102;
const CMD_SET_SCANOUT: u32 = 0x103;
const CMD_RESOURCE_FLUSH: u32 = 0x104;
const CMD_TRANSFER_TO_HOST_2D: u32 = 0x105;
const CMD_ATTACH_BACKING: u32 = 0x106;
const CMD_DETACH_BACKING: u32 = 0x107;
const CMD_GET_CAPSET_INFO: u32 = 0x108;
const CMD_GET_CAPSET: u32 = 0x109;
const CMD_CTX_CREATE: u32 = 0x200;
const CMD_CTX_DESTROY: u32 = 0x201;
const CMD_CTX_ATTACH: u32 = 0x202;
const CMD_CTX_DETACH: u32 = 0x203;
const CMD_RESOURCE_CREATE_3D: u32 = 0x204;
const CMD_TRANSFER_TO_HOST_3D: u32 = 0x205;
const CMD_TRANSFER_FROM_HOST_3D: u32 = 0x206;
const CMD_SUBMIT_3D: u32 = 0x207;

const RESP_NODATA: u32 = 0x1100;
const RESP_DISPLAY_INFO: u32 = 0x1101;
const RESP_CAPSET_INFO: u32 = 0x1102;
const RESP_CAPSET: u32 = 0x1103;
const RESP_ERR_UNSPEC: u32 = 0x1200;

/// The two capability sets, `VIRGL` then `VIRGL2`.
const CAPSETS: [u32; 2] = [1, 2];

const TARGET_TEXTURE_2D: u32 = 2;
const BIND_RENDER_TARGET: u32 = 1 << 1;
const FLAG_Y_0_TOP: u32 = 1;

const FORMAT_R8G8B8A8: u32 = 67;
const FORMAT_R8G8B8X8: u32 = 134;

/// What the device needs from a renderer.
pub trait Backend {
    /// Readies the renderer for a request.
    fn prepare(&mut self);
    /// Finishes queued work.
    fn poll(&mut self);
    /// The highest version and size of a capability set.
    fn capset_info(&mut self, id: u32) -> (u32, u32);
    /// The contents of a capability set.
    fn capset(&mut self, id: u32, version: u32) -> Vec<u8>;
    /// Creates a resource, returning a status.
    fn create_resource(&mut self, args: ResourceArgs) -> i32;
    /// Destroys a resource.
    fn destroy_resource(&mut self, handle: u32);
    /// Gives a resource guest memory, as `(address, length)` entries.
    fn attach_backing(&mut self, handle: u32, entries: &[(u64, u32)]) -> i32;
    /// Takes a resource's guest memory away.
    fn detach_backing(&mut self, handle: u32);
    /// Creates a rendering context.
    fn create_context(&mut self, id: u32, name: &[u8]) -> i32;
    /// Destroys a rendering context.
    fn destroy_context(&mut self, id: u32);
    /// Lets a context use a resource.
    fn attach_to_context(&mut self, context: u32, resource: u32);
    /// Stops a context using a resource.
    fn detach_from_context(&mut self, context: u32, resource: u32);
    /// Runs a command buffer.
    fn submit(&mut self, context: u32, commands: &mut [u32]) -> i32;
    /// Copies guest memory to the host.
    fn transfer_to_host(&mut self, transfer: Transfer) -> i32;
    /// Copies the host's copy to guest memory.
    fn transfer_from_host(&mut self, transfer: Transfer) -> i32;
}

impl Backend for Renderer<'_> {
    fn prepare(&mut self) {
        Renderer::prepare(self);
    }
    fn poll(&mut self) {
        Renderer::poll(self);
    }
    fn capset_info(&mut self, id: u32) -> (u32, u32) {
        Renderer::capset_info(self, id)
    }
    fn capset(&mut self, id: u32, version: u32) -> Vec<u8> {
        Renderer::capset(self, id, version)
    }
    fn create_resource(&mut self, args: ResourceArgs) -> i32 {
        Renderer::create_resource(self, args)
    }
    fn destroy_resource(&mut self, handle: u32) {
        Renderer::destroy_resource(self, handle);
    }
    fn attach_backing(&mut self, handle: u32, entries: &[(u64, u32)]) -> i32 {
        Renderer::attach_backing(self, handle, entries)
    }
    fn detach_backing(&mut self, handle: u32) {
        Renderer::detach_backing(self, handle);
    }
    fn create_context(&mut self, id: u32, name: &[u8]) -> i32 {
        Renderer::create_context(self, id, name)
    }
    fn destroy_context(&mut self, id: u32) {
        Renderer::destroy_context(self, id);
    }
    fn attach_to_context(&mut self, context: u32, resource: u32) {
        Renderer::attach_to_context(self, context, resource);
    }
    fn detach_from_context(&mut self, context: u32, resource: u32) {
        Renderer::detach_from_context(self, context, resource);
    }
    fn submit(&mut self, context: u32, commands: &mut [u32]) -> i32 {
        Renderer::submit(self, context, commands)
    }
    fn transfer_to_host(&mut self, transfer: Transfer) -> i32 {
        Renderer::transfer_to_host(self, transfer)
    }
    fn transfer_from_host(&mut self, transfer: Transfer) -> i32 {
        Renderer::transfer_from_host(self, transfer)
    }
}

/// What the guest is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scanout {
    /// The resource on screen.
    pub resource: u32,
    /// Its width in pixels.
    pub width: u32,
    /// Its height in pixels.
    pub height: u32,
    /// Its bytes run red first, where the window wants blue first.
    pub swap_red_blue: bool,
}

/// Counts of GPU activity, for reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct GpuStats {
    /// Requests handled.
    pub commands: u64,
    /// Requests the renderer refused.
    pub errors: u64,
    /// Command buffers run.
    pub submits: u64,
    /// Frames the guest presented.
    pub flushes: u64,
}

#[derive(Debug, Clone, Copy)]
struct Resource {
    format: u32,
    width: u32,
    height: u32,
}

/// A little-endian reader over a request. Reads past the end give `None`.
struct Fields<'a>(&'a [u8]);

impl Fields<'_> {
    fn word(&self, at: usize) -> Option<u32> {
        let bytes = self.0.get(at..at + 4)?;
        Some(u32::from_le_bytes(bytes.try_into().ok()?))
    }

    fn long(&self, at: usize) -> Option<u64> {
        let bytes = self.0.get(at..at + 8)?;
        Some(u64::from_le_bytes(bytes.try_into().ok()?))
    }
}

type Answer = Option<(u32, Vec<u8>)>;

fn status(code: i32) -> Answer {
    (code == 0).then(|| (RESP_NODATA, Vec::new()))
}

/// Copies a request out of guest memory. `None` if it is too long.
fn gather(memory: &dyn GuestMemory, chain: &Chain) -> Result<Option<Vec<u8>>, RingError> {
    let total: usize = chain.readable.iter().map(|part| part.1 as usize).sum();
    if total > MAX_REQUEST {
        return Ok(None);
    }
    let mut request = vec![0_u8; total];
    let mut at = 0;
    for &(address, length) in &chain.readable {
        let end = at + length as usize;
        memory.read(address, &mut request[at..end])?;
        at = end;
    }
    Ok(Some(request))
}

/// Writes a response across the guest's buffers. Returns the bytes written.
fn scatter(memory: &dyn GuestMemory, chain: &Chain, response: &[u8]) -> Result<u32, RingError> {
    let mut rest = response;
    let mut written = 0;
    for &(address, length) in &chain.writable {
        let count = rest.len().min(length as usize);
        let (now, later) = rest.split_at(count);
        memory.write(address, now)?;
        rest = later;
        written += count as u32;
    }
    Ok(written)
}

/// The GPU.
pub struct VirtioGpu {
    common: Common,
    width: u32,
    height: u32,
    resources: HashMap<u32, Resource>,
    scanout: u32,
    frame_waiting: bool,
    irq_requested: bool,
    stats: GpuStats,
}

impl VirtioGpu {
    /// Creates a GPU with one screen of this size.
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            common: Common::new(QUEUES, QUEUE_SIZE, VERSION_1 | FEATURE_VIRGL),
            width,
            height,
            resources: HashMap::new(),
            scanout: 0,
            frame_waiting: false,
            irq_requested: false,
            stats: GpuStats::default(),
        }
    }

    /// Returns true once for each interrupt since the last call.
    pub fn take_irq(&mut self) -> bool {
        std::mem::take(&mut self.irq_requested)
    }

    /// Returns true once for each batch of new frames.
    pub fn take_frame_waiting(&mut self) -> bool {
        std::mem::take(&mut self.frame_waiting)
    }

    /// What is on screen.
    #[must_use]
    pub fn scanout(&self) -> Option<Scanout> {
        let resource = self.resources.get(&self.scanout)?;
        Some(Scanout {
            resource: self.scanout,
            width: resource.width,
            height: resource.height,
            swap_red_blue: matches!(resource.format, FORMAT_R8G8B8A8 | FORMAT_R8G8B8X8),
        })
    }

    /// Activity counters.
    #[must_use]
    pub fn stats(&self) -> GpuStats {
        self.stats
    }

    /// Handles a read of the device's memory.
    pub fn read(&mut self, offset: u64, size: u8) -> u32 {
        match offset {
            ISR_OFFSET => u32::from(self.common.take_isr()),
            NOTIFY_OFFSET..ISR_OFFSET => 0,
            DEVICE_OFFSET.. => match offset - DEVICE_OFFSET {
                8 => 1,                     // One screen.
                12 => CAPSETS.len() as u32, // Capability sets.
                _ => 0,                     // No display events.
            },
            _ => self.common.read(offset, size),
        }
    }

    /// Handles a write to the device's memory. Returns true for a
    /// notification, after which the caller must run [`Self::service`].
    pub fn write(&mut self, offset: u64, value: u32) -> bool {
        if offset < NOTIFY_OFFSET {
            if self.common.write(offset, value) {
                *self = Self::new(self.width, self.height);
            }
            return false;
        }
        offset == NOTIFY_OFFSET
    }

    /// Runs everything the guest has queued. Call while the vCPU is stopped.
    pub fn service(&mut self, memory: &dyn GuestMemory, backend: &mut dyn Backend) {
        match self.drain(memory, backend) {
            Ok(true) => {
                self.common.signal_queue();
                self.irq_requested = true;
            }
            Ok(false) => {}
            Err(RingError) => self.common.fail(),
        }
    }

    fn drain(
        &mut self,
        memory: &dyn GuestMemory,
        backend: &mut dyn Backend,
    ) -> Result<bool, RingError> {
        let mut used = false;
        for index in 0..QUEUES {
            let mut queue = self.common.queue(index);
            while let Some(chain) = next_chain(memory, &mut queue)? {
                // The cursor queue has no answers, and RAX draws no cursor.
                let written = if index == CONTROL_QUEUE {
                    let request = gather(memory, &chain)?.unwrap_or_default();
                    let response = self.command(&request, backend);
                    scatter(memory, &chain, &response)?
                } else {
                    0
                };
                complete(memory, &queue, chain.head, written)?;
                self.common.set_queue(index, queue);
                used = true;
            }
        }
        if used {
            backend.poll();
        }
        Ok(used)
    }

    /// Answers one request. Its header comes back with the same fence.
    fn command(&mut self, request: &[u8], backend: &mut dyn Backend) -> Vec<u8> {
        let fields = Fields(request);
        self.stats.commands += 1;
        backend.prepare();
        let answer = self.dispatch(&fields, backend);
        if answer.is_none() {
            self.stats.errors += 1;
        }
        let (kind, body) = answer.unwrap_or((RESP_ERR_UNSPEC, Vec::new()));
        let flags = fields.word(4).unwrap_or(0) & FLAG_FENCE;
        let fence = fields.long(8).unwrap_or(0);
        let context = fields.word(16).unwrap_or(0);
        let mut response = Vec::with_capacity(HEADER_BYTES + body.len());
        response.extend_from_slice(&kind.to_le_bytes());
        response.extend_from_slice(&flags.to_le_bytes());
        response.extend_from_slice(&fence.to_le_bytes());
        response.extend_from_slice(&context.to_le_bytes());
        response.extend_from_slice(&0_u32.to_le_bytes());
        response.extend_from_slice(&body);
        response
    }

    fn dispatch(&mut self, request: &Fields<'_>, backend: &mut dyn Backend) -> Answer {
        let context = request.word(16)?;
        match request.word(0)? {
            CMD_GET_DISPLAY_INFO => Some((RESP_DISPLAY_INFO, self.display_info())),
            CMD_RESOURCE_CREATE_2D => {
                let handle = request.word(24)?;
                let args = ResourceArgs {
                    handle,
                    target: TARGET_TEXTURE_2D,
                    format: request.word(28)?,
                    bind: BIND_RENDER_TARGET,
                    width: request.word(32)?,
                    height: request.word(36)?,
                    depth: 1,
                    array_size: 1,
                    last_level: 0,
                    samples: 0,
                    flags: FLAG_Y_0_TOP,
                };
                self.create(args, backend)
            }
            CMD_RESOURCE_CREATE_3D => {
                let args = ResourceArgs {
                    handle: request.word(24)?,
                    target: request.word(28)?,
                    format: request.word(32)?,
                    bind: request.word(36)?,
                    width: request.word(40)?,
                    height: request.word(44)?,
                    depth: request.word(48)?,
                    array_size: request.word(52)?,
                    last_level: request.word(56)?,
                    samples: request.word(60)?,
                    flags: request.word(64)?,
                };
                self.create(args, backend)
            }
            CMD_RESOURCE_UNREF => {
                let handle = request.word(24)?;
                self.resources.remove(&handle);
                if self.scanout == handle {
                    self.scanout = 0;
                }
                backend.destroy_resource(handle);
                status(0)
            }
            CMD_SET_SCANOUT => {
                if request.word(40)? != 0 {
                    return None; // There is only one screen.
                }
                self.scanout = request.word(44)?;
                self.frame_waiting = true;
                status(0)
            }
            CMD_RESOURCE_FLUSH => {
                if request.word(40)? == self.scanout {
                    self.frame_waiting = true;
                    self.stats.flushes += 1;
                }
                status(0)
            }
            CMD_TRANSFER_TO_HOST_2D => {
                let transfer = Transfer {
                    resource: request.word(48)?,
                    context: 0,
                    level: 0,
                    stride: 0,
                    layer_stride: 0,
                    region: TransferBox {
                        x: request.word(24)?,
                        y: request.word(28)?,
                        z: 0,
                        width: request.word(32)?,
                        height: request.word(36)?,
                        depth: 1,
                    },
                    offset: request.long(40)?,
                };
                status(backend.transfer_to_host(transfer))
            }
            CMD_TRANSFER_TO_HOST_3D => {
                status(backend.transfer_to_host(transfer_3d(request, context)?))
            }
            CMD_TRANSFER_FROM_HOST_3D => {
                status(backend.transfer_from_host(transfer_3d(request, context)?))
            }
            CMD_ATTACH_BACKING => {
                let count = request.word(28)? as usize;
                let mut entries = Vec::with_capacity(count.min(request.0.len() / 16));
                for index in 0..count {
                    let at = index.checked_mul(16)?.checked_add(32)?;
                    entries.push((request.long(at)?, request.word(at + 8)?));
                }
                status(backend.attach_backing(request.word(24)?, &entries))
            }
            CMD_DETACH_BACKING => {
                backend.detach_backing(request.word(24)?);
                status(0)
            }
            CMD_GET_CAPSET_INFO => {
                let id = *CAPSETS.get(request.word(24)? as usize)?;
                let (version, size) = backend.capset_info(id);
                let mut body = Vec::with_capacity(16);
                for word in [id, version, size, 0] {
                    body.extend_from_slice(&word.to_le_bytes());
                }
                Some((RESP_CAPSET_INFO, body))
            }
            CMD_GET_CAPSET => {
                let id = request.word(24)?;
                CAPSETS.contains(&id).then_some(())?;
                Some((RESP_CAPSET, backend.capset(id, request.word(28)?)))
            }
            CMD_CTX_CREATE => {
                let length = (request.word(24)? as usize).min(64);
                let name = request.0.get(32..32 + length)?;
                status(backend.create_context(context, name))
            }
            CMD_CTX_DESTROY => {
                backend.destroy_context(context);
                status(0)
            }
            CMD_CTX_ATTACH => {
                backend.attach_to_context(context, request.word(24)?);
                status(0)
            }
            CMD_CTX_DETACH => {
                backend.detach_from_context(context, request.word(24)?);
                status(0)
            }
            CMD_SUBMIT_3D => {
                let length = request.word(24)? as usize;
                let end = length.checked_add(32)?;
                let bytes = request.0.get(32..end)?;
                let (words, _) = bytes.as_chunks::<4>();
                let mut commands: Vec<u32> =
                    words.iter().copied().map(u32::from_le_bytes).collect();
                self.stats.submits += 1;
                status(backend.submit(context, &mut commands))
            }
            _ => None,
        }
    }

    fn create(&mut self, args: ResourceArgs, backend: &mut dyn Backend) -> Answer {
        let answer = status(backend.create_resource(args));
        if answer.is_some() {
            self.resources.insert(
                args.handle,
                Resource {
                    format: args.format,
                    width: args.width,
                    height: args.height,
                },
            );
        }
        answer
    }

    fn display_info(&self) -> Vec<u8> {
        let mut body = vec![0_u8; SCANOUTS * 24];
        let first = [0, 0, self.width, self.height, 1, 0];
        for (at, word) in first.into_iter().enumerate() {
            body[at * 4..at * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        body
    }
}

fn transfer_3d(request: &Fields<'_>, context: u32) -> Option<Transfer> {
    Some(Transfer {
        resource: request.word(56)?,
        context,
        level: request.word(60)?,
        stride: request.word(64)?,
        layer_stride: request.word(68)?,
        region: TransferBox {
            x: request.word(24)?,
            y: request.word(28)?,
            z: request.word(32)?,
            width: request.word(36)?,
            height: request.word(40)?,
            depth: request.word(44)?,
        },
        offset: request.long(48)?,
    })
}

/// Swaps the first and third byte of every pixel.
pub fn swap_red_blue(pixels: &mut [u8]) {
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
}

/// Copies the top left of one image into another of a different size.
pub fn copy_cropped(source: &[u8], source_width: u32, target: &mut [u8], target_width: u32) {
    let source_row = source_width as usize * 4;
    let target_row = target_width as usize * 4;
    if source_row == 0 || target_row == 0 {
        return;
    }
    let row = source_row.min(target_row);
    let rows = source.chunks_exact(source_row);
    for (from, to) in rows.zip(target.chunks_exact_mut(target_row)) {
        to[..row].copy_from_slice(&from[..row]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio_modern::testing::{AVAILABLE, DESCRIPTORS, Ram, USED};

    const REQUEST: u64 = 0x4_0000;
    const RESPONSE: u64 = 0x6_0000;

    #[derive(Default)]
    struct Fake {
        calls: Vec<String>,
        refuse: bool,
    }

    impl Fake {
        fn result(&self) -> i32 {
            i32::from(self.refuse)
        }
    }

    impl Backend for Fake {
        fn prepare(&mut self) {}
        fn poll(&mut self) {
            self.calls.push("poll".to_owned());
        }
        fn capset_info(&mut self, id: u32) -> (u32, u32) {
            (id, id * 100)
        }
        fn capset(&mut self, id: u32, version: u32) -> Vec<u8> {
            vec![id as u8, version as u8]
        }
        fn create_resource(&mut self, args: ResourceArgs) -> i32 {
            self.calls.push(format!("create {args:?}"));
            self.result()
        }
        fn destroy_resource(&mut self, handle: u32) {
            self.calls.push(format!("destroy {handle}"));
        }
        fn attach_backing(&mut self, handle: u32, entries: &[(u64, u32)]) -> i32 {
            self.calls.push(format!("attach {handle} {entries:?}"));
            self.result()
        }
        fn detach_backing(&mut self, handle: u32) {
            self.calls.push(format!("detach {handle}"));
        }
        fn create_context(&mut self, id: u32, name: &[u8]) -> i32 {
            let name = String::from_utf8_lossy(name);
            self.calls.push(format!("context {id} {name}"));
            self.result()
        }
        fn destroy_context(&mut self, id: u32) {
            self.calls.push(format!("end context {id}"));
        }
        fn attach_to_context(&mut self, context: u32, resource: u32) {
            self.calls.push(format!("use {context} {resource}"));
        }
        fn detach_from_context(&mut self, context: u32, resource: u32) {
            self.calls.push(format!("unuse {context} {resource}"));
        }
        fn submit(&mut self, context: u32, commands: &mut [u32]) -> i32 {
            self.calls.push(format!("submit {context} {commands:?}"));
            self.result()
        }
        fn transfer_to_host(&mut self, transfer: Transfer) -> i32 {
            self.calls.push(format!("to host {transfer:?}"));
            self.result()
        }
        fn transfer_from_host(&mut self, transfer: Transfer) -> i32 {
            self.calls.push(format!("from host {transfer:?}"));
            self.result()
        }
    }

    fn device() -> VirtioGpu {
        let mut gpu = VirtioGpu::new(1280, 720);
        gpu.write(0x16, 0);
        gpu.write(0x18, 8);
        gpu.write(0x20, DESCRIPTORS as u32);
        gpu.write(0x28, AVAILABLE as u32);
        gpu.write(0x30, USED as u32);
        gpu.write(0x1c, 1);
        gpu
    }

    /// Builds a request from a type, context and words after the header.
    fn request(kind: u32, context: u32, words: &[u32]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for word in [kind, FLAG_FENCE, 77, 0, context, 0] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        for word in words {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes
    }

    /// Runs one request through the ring. Returns the response, which is
    /// empty on the guest's side when `position` was not consumed.
    fn run(
        gpu: &mut VirtioGpu,
        ram: &Ram,
        backend: &mut Fake,
        position: u16,
        bytes: &[u8],
    ) -> Vec<u8> {
        ram.write(REQUEST, bytes).unwrap();
        ram.descriptor(0, REQUEST, bytes.len() as u32, false, Some(1));
        ram.descriptor(1, RESPONSE, 0x1_0000, true, None);
        ram.offer(position, 0);
        gpu.service(ram, backend);
        let mut response = vec![0; 2048];
        ram.read(RESPONSE, &mut response).unwrap();
        response
    }

    fn word(bytes: &[u8], at: usize) -> u32 {
        Fields(bytes).word(at).unwrap()
    }

    #[test]
    fn it_offers_virgl_and_version_1() {
        let mut gpu = VirtioGpu::new(640, 480);
        assert_eq!(gpu.read(0x04, 4), 1);
        gpu.write(0x00, 1);
        assert_eq!(gpu.read(0x04, 4), 1);
    }

    #[test]
    fn the_config_reports_one_screen_and_two_capsets() {
        let mut gpu = VirtioGpu::new(640, 480);
        assert_eq!(gpu.read(DEVICE_OFFSET, 4), 0);
        assert_eq!(gpu.read(DEVICE_OFFSET + 8, 4), 1);
        assert_eq!(gpu.read(DEVICE_OFFSET + 12, 4), 2);
    }

    #[test]
    fn only_the_notify_register_asks_for_service() {
        let mut gpu = VirtioGpu::new(640, 480);
        assert!(gpu.write(NOTIFY_OFFSET, 0));
        assert!(!gpu.write(0x16, 0));
        assert!(!gpu.write(DEVICE_OFFSET + 4, 1));
    }

    #[test]
    fn the_display_size_reaches_the_guest_with_its_fence() {
        let (mut gpu, ram, mut fake) = (device(), Ram::new(0x8_0000), Fake::default());
        let out = run(
            &mut gpu,
            &ram,
            &mut fake,
            0,
            &request(CMD_GET_DISPLAY_INFO, 0, &[]),
        );
        assert_eq!(word(&out, 0), RESP_DISPLAY_INFO);
        assert_eq!(word(&out, 4), FLAG_FENCE);
        assert_eq!(word(&out, 8), 77);
        assert_eq!(word(&out, 24 + 8), 1280);
        assert_eq!(word(&out, 24 + 12), 720);
        assert_eq!(word(&out, 24 + 16), 1, "enabled");
        assert!(gpu.take_irq());
        assert_eq!(gpu.read(ISR_OFFSET, 1), 1);
    }

    #[test]
    fn capset_lookup_goes_by_index_then_by_id() {
        let (mut gpu, ram, mut fake) = (device(), Ram::new(0x8_0000), Fake::default());
        let out = run(
            &mut gpu,
            &ram,
            &mut fake,
            0,
            &request(CMD_GET_CAPSET_INFO, 0, &[1, 0]),
        );
        assert_eq!(word(&out, 0), RESP_CAPSET_INFO);
        assert_eq!(word(&out, 24), 2);
        assert_eq!(word(&out, 28), 2);
        assert_eq!(word(&out, 32), 200);
        let out = run(
            &mut gpu,
            &ram,
            &mut fake,
            1,
            &request(CMD_GET_CAPSET, 0, &[2, 5]),
        );
        assert_eq!(word(&out, 0), RESP_CAPSET);
        assert_eq!(out[24..26], [2, 5]);
    }

    #[test]
    fn an_unknown_capset_is_refused() {
        let (mut gpu, ram, mut fake) = (device(), Ram::new(0x8_0000), Fake::default());
        let out = run(
            &mut gpu,
            &ram,
            &mut fake,
            0,
            &request(CMD_GET_CAPSET, 0, &[9, 0]),
        );
        assert_eq!(word(&out, 0), RESP_ERR_UNSPEC);
    }

    #[test]
    fn a_2d_resource_is_a_top_down_render_target() {
        let (mut gpu, ram, mut fake) = (device(), Ram::new(0x8_0000), Fake::default());
        let bytes = request(CMD_RESOURCE_CREATE_2D, 0, &[3, 1, 64, 32]);
        let out = run(&mut gpu, &ram, &mut fake, 0, &bytes);
        assert_eq!(word(&out, 0), RESP_NODATA);
        let expected = ResourceArgs {
            handle: 3,
            target: 2,
            format: 1,
            bind: 2,
            width: 64,
            height: 32,
            depth: 1,
            array_size: 1,
            last_level: 0,
            samples: 0,
            flags: 1,
        };
        assert_eq!(fake.calls[0], format!("create {expected:?}"));
    }

    #[test]
    fn a_3d_resource_keeps_every_guest_field() {
        let (mut gpu, ram, mut fake) = (device(), Ram::new(0x8_0000), Fake::default());
        let words = [3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 0];
        let bytes = request(CMD_RESOURCE_CREATE_3D, 0, &words);
        run(&mut gpu, &ram, &mut fake, 0, &bytes);
        let expected = ResourceArgs {
            handle: 3,
            target: 4,
            format: 5,
            bind: 6,
            width: 7,
            height: 8,
            depth: 9,
            array_size: 10,
            last_level: 11,
            samples: 12,
            flags: 13,
        };
        assert_eq!(fake.calls[0], format!("create {expected:?}"));
    }

    #[test]
    fn a_refused_resource_cannot_become_the_screen() {
        let (mut gpu, ram) = (device(), Ram::new(0x8_0000));
        let mut fake = Fake {
            refuse: true,
            ..Fake::default()
        };
        let bytes = request(CMD_RESOURCE_CREATE_2D, 0, &[3, 1, 64, 32]);
        let out = run(&mut gpu, &ram, &mut fake, 0, &bytes);
        assert_eq!(word(&out, 0), RESP_ERR_UNSPEC);
        let bytes = request(CMD_SET_SCANOUT, 0, &[0, 0, 64, 32, 0, 3]);
        run(&mut gpu, &ram, &mut fake, 1, &bytes);
        assert_eq!(gpu.scanout(), None);
        assert_eq!(gpu.stats().errors, 1);
    }

    #[test]
    fn scanout_follows_flushes_and_unrefs() {
        let (mut gpu, ram, mut fake) = (device(), Ram::new(0x8_0000), Fake::default());
        let bytes = request(CMD_RESOURCE_CREATE_2D, 0, &[3, 67, 64, 32]);
        run(&mut gpu, &ram, &mut fake, 0, &bytes);
        let bytes = request(CMD_SET_SCANOUT, 0, &[0, 0, 64, 32, 0, 3]);
        run(&mut gpu, &ram, &mut fake, 1, &bytes);
        let shown = Scanout {
            resource: 3,
            width: 64,
            height: 32,
            swap_red_blue: true,
        };
        assert_eq!(gpu.scanout(), Some(shown));
        assert!(gpu.take_frame_waiting());
        assert!(!gpu.take_frame_waiting());

        let bytes = request(CMD_RESOURCE_FLUSH, 0, &[0, 0, 64, 32, 9, 0]);
        run(&mut gpu, &ram, &mut fake, 2, &bytes);
        assert!(!gpu.take_frame_waiting(), "a resource not on screen");
        let bytes = request(CMD_RESOURCE_FLUSH, 0, &[0, 0, 64, 32, 3, 0]);
        run(&mut gpu, &ram, &mut fake, 3, &bytes);
        assert!(gpu.take_frame_waiting());
        assert_eq!(gpu.stats().flushes, 1);

        let bytes = request(CMD_RESOURCE_UNREF, 0, &[3, 0]);
        run(&mut gpu, &ram, &mut fake, 4, &bytes);
        assert_eq!(gpu.scanout(), None);
        assert!(fake.calls.contains(&"destroy 3".to_owned()));
    }

    #[test]
    fn backing_entries_are_unpacked() {
        let (mut gpu, ram, mut fake) = (device(), Ram::new(0x8_0000), Fake::default());
        let words = [3, 2, 0x1000, 0, 0x2000, 0, 0x5000, 1, 0x100, 0];
        let bytes = request(CMD_ATTACH_BACKING, 0, &words);
        let out = run(&mut gpu, &ram, &mut fake, 0, &bytes);
        assert_eq!(word(&out, 0), RESP_NODATA);
        assert_eq!(
            fake.calls[0],
            format!(
                "attach 3 {:?}",
                [(0x1000_u64, 0x2000_u32), (0x1_0000_5000, 0x100)]
            )
        );
    }

    #[test]
    fn a_backing_count_longer_than_the_request_is_refused() {
        let (mut gpu, ram, mut fake) = (device(), Ram::new(0x8_0000), Fake::default());
        let bytes = request(CMD_ATTACH_BACKING, 0, &[3, u32::MAX]);
        let out = run(&mut gpu, &ram, &mut fake, 0, &bytes);
        assert_eq!(word(&out, 0), RESP_ERR_UNSPEC);
        assert!(!fake.calls.iter().any(|call| call.starts_with("attach")));
    }

    #[test]
    fn a_3d_transfer_keeps_its_box_and_context() {
        let (mut gpu, ram, mut fake) = (device(), Ram::new(0x8_0000), Fake::default());
        let words = [1, 2, 3, 4, 5, 6, 0x10, 0, 9, 1, 40, 80];
        let bytes = request(CMD_TRANSFER_TO_HOST_3D, 7, &words);
        run(&mut gpu, &ram, &mut fake, 0, &bytes);
        let expected = Transfer {
            resource: 9,
            context: 7,
            level: 1,
            stride: 40,
            layer_stride: 80,
            region: TransferBox {
                x: 1,
                y: 2,
                z: 3,
                width: 4,
                height: 5,
                depth: 6,
            },
            offset: 0x10,
        };
        assert_eq!(fake.calls[0], format!("to host {expected:?}"));
    }

    #[test]
    fn a_2d_transfer_has_no_context_and_one_layer() {
        let (mut gpu, ram, mut fake) = (device(), Ram::new(0x8_0000), Fake::default());
        let words = [1, 2, 3, 4, 0x20, 0, 9, 0];
        let bytes = request(CMD_TRANSFER_TO_HOST_2D, 0, &words);
        run(&mut gpu, &ram, &mut fake, 0, &bytes);
        let expected = Transfer {
            resource: 9,
            context: 0,
            level: 0,
            stride: 0,
            layer_stride: 0,
            region: TransferBox {
                x: 1,
                y: 2,
                z: 0,
                width: 3,
                height: 4,
                depth: 1,
            },
            offset: 0x20,
        };
        assert_eq!(fake.calls[0], format!("to host {expected:?}"));
    }

    #[test]
    fn commands_reach_the_renderer_as_words() {
        let (mut gpu, ram, mut fake) = (device(), Ram::new(0x8_0000), Fake::default());
        let bytes = request(CMD_SUBMIT_3D, 5, &[8, 0, 0xaa, 0xbb]);
        let out = run(&mut gpu, &ram, &mut fake, 0, &bytes);
        assert_eq!(word(&out, 0), RESP_NODATA);
        assert_eq!(fake.calls[0], format!("submit 5 {:?}", [0xaa_u32, 0xbb]));
        assert_eq!(gpu.stats().submits, 1);
    }

    #[test]
    fn a_submit_longer_than_its_request_is_refused() {
        let (mut gpu, ram, mut fake) = (device(), Ram::new(0x8_0000), Fake::default());
        let bytes = request(CMD_SUBMIT_3D, 5, &[u32::MAX, 0]);
        let out = run(&mut gpu, &ram, &mut fake, 0, &bytes);
        assert_eq!(word(&out, 0), RESP_ERR_UNSPEC);
        assert_eq!(gpu.stats().submits, 0);
    }

    #[test]
    fn a_context_gets_its_name_and_a_bounded_length() {
        let (mut gpu, ram, mut fake) = (device(), Ram::new(0x8_0000), Fake::default());
        let mut bytes = request(CMD_CTX_CREATE, 5, &[2, 0]);
        bytes.extend_from_slice(b"hi");
        bytes.resize(bytes.len() + 62, 0);
        run(&mut gpu, &ram, &mut fake, 0, &bytes);
        assert_eq!(fake.calls[0], "context 5 hi");
        let mut bytes = request(CMD_CTX_CREATE, 6, &[u32::MAX, 0]);
        bytes.resize(bytes.len() + 64, b'x');
        let out = run(&mut gpu, &ram, &mut fake, 1, &bytes);
        assert_eq!(word(&out, 0), RESP_NODATA);
    }

    #[test]
    fn contexts_attach_and_detach_resources() {
        let (mut gpu, ram, mut fake) = (device(), Ram::new(0x8_0000), Fake::default());
        run(
            &mut gpu,
            &ram,
            &mut fake,
            0,
            &request(CMD_CTX_ATTACH, 5, &[9, 0]),
        );
        run(
            &mut gpu,
            &ram,
            &mut fake,
            1,
            &request(CMD_CTX_DETACH, 5, &[9, 0]),
        );
        run(
            &mut gpu,
            &ram,
            &mut fake,
            2,
            &request(CMD_CTX_DESTROY, 5, &[]),
        );
        assert!(fake.calls.contains(&"use 5 9".to_owned()));
        assert!(fake.calls.contains(&"unuse 5 9".to_owned()));
        assert!(fake.calls.contains(&"end context 5".to_owned()));
    }

    #[test]
    fn unknown_and_short_requests_get_an_error_not_a_panic() {
        let (mut gpu, ram, mut fake) = (device(), Ram::new(0x8_0000), Fake::default());
        let out = run(&mut gpu, &ram, &mut fake, 0, &request(0x999, 0, &[]));
        assert_eq!(word(&out, 0), RESP_ERR_UNSPEC);
        let out = run(&mut gpu, &ram, &mut fake, 1, &[1, 2, 3]);
        assert_eq!(word(&out, 0), RESP_ERR_UNSPEC);
        let bytes = request(CMD_RESOURCE_CREATE_3D, 0, &[3]);
        let out = run(&mut gpu, &ram, &mut fake, 2, &bytes);
        assert_eq!(word(&out, 0), RESP_ERR_UNSPEC);
    }

    #[test]
    fn an_oversized_request_is_refused_without_reading_it() {
        let (mut gpu, ram, mut fake) = (device(), Ram::new(0x8_0000), Fake::default());
        ram.descriptor(0, 0, MAX_REQUEST as u32 + 1, false, Some(1));
        ram.descriptor(1, RESPONSE, 64, true, None);
        ram.offer(0, 0);
        gpu.service(&ram, &mut fake);
        let mut out = [0; 24];
        ram.read(RESPONSE, &mut out).unwrap();
        assert_eq!(word(&out, 0), RESP_ERR_UNSPEC);
    }

    #[test]
    fn a_request_outside_ram_stops_the_device() {
        let (mut gpu, ram, mut fake) = (device(), Ram::new(0x8_0000), Fake::default());
        ram.descriptor(0, 0xffff_0000, 64, false, None);
        ram.offer(0, 0);
        gpu.service(&ram, &mut fake);
        assert_ne!(gpu.read(0x14, 1) & 0x80, 0);
    }

    #[test]
    fn the_cursor_queue_completes_without_an_answer() {
        let (mut gpu, ram, mut fake) = (device(), Ram::new(0x8_0000), Fake::default());
        gpu.write(0x16, 1);
        gpu.write(0x18, 8);
        gpu.write(0x20, DESCRIPTORS as u32);
        gpu.write(0x28, AVAILABLE as u32);
        gpu.write(0x30, USED as u32);
        gpu.write(0x1c, 1);
        gpu.write(0x16, 0);
        gpu.write(0x1c, 0);
        ram.descriptor(0, REQUEST, 56, false, None);
        ram.offer(0, 0);
        gpu.service(&ram, &mut fake);
        assert_eq!(ram.used_count(), 1);
        assert_eq!(gpu.stats().commands, 0);
    }

    #[test]
    fn an_idle_device_does_not_interrupt() {
        let (mut gpu, ram, mut fake) = (device(), Ram::new(0x8_0000), Fake::default());
        gpu.service(&ram, &mut fake);
        assert!(!gpu.take_irq());
        assert!(fake.calls.is_empty());
    }

    #[test]
    fn a_reset_forgets_resources() {
        let (mut gpu, ram, mut fake) = (device(), Ram::new(0x8_0000), Fake::default());
        let bytes = request(CMD_RESOURCE_CREATE_2D, 0, &[3, 1, 64, 32]);
        run(&mut gpu, &ram, &mut fake, 0, &bytes);
        let bytes = request(CMD_SET_SCANOUT, 0, &[0, 0, 64, 32, 0, 3]);
        run(&mut gpu, &ram, &mut fake, 1, &bytes);
        gpu.write(0x14, 0);
        assert_eq!(gpu.scanout(), None);
        assert_eq!(gpu.read(0x1c, 2), 0);
    }

    #[test]
    fn red_and_blue_swap() {
        let mut pixels = [1, 2, 3, 4, 5, 6, 7, 8];
        swap_red_blue(&mut pixels);
        assert_eq!(pixels, [3, 2, 1, 4, 7, 6, 5, 8]);
    }

    #[test]
    fn a_wider_source_is_cropped_row_by_row() {
        let source = [1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4];
        let mut target = [0; 8];
        copy_cropped(&source, 2, &mut target, 1);
        assert_eq!(target, [1, 1, 1, 1, 3, 3, 3, 3]);
    }

    #[test]
    fn an_empty_image_copies_nothing() {
        let mut target = [9; 4];
        copy_cropped(&[], 0, &mut target, 1);
        assert_eq!(target, [9; 4]);
    }
}
