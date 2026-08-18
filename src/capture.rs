//! Minimal wlr-screencopy screenshot client.
//!
//! woomer only needs two things from the compositor: the list of outputs with
//! their logical geometry, and one output's pixels as RGBA. grim-rs provided
//! that but hard-depends on `image` (with every codec enabled) and `chrono`
//! for its PNG/CLI features, so this module talks the three relevant Wayland
//! protocols (wl_output + xdg-output, wlr-screencopy, wl_shm) directly.

use std::fs::File;
use std::io::Read;
use std::os::fd::AsFd;
use std::sync::{Arc, Mutex};

use wayland_client::{
    protocol::{
        wl_buffer::WlBuffer,
        wl_output::{self, Transform, WlOutput},
        wl_registry::{self, WlRegistry},
        wl_shm::{Format as ShmFormat, WlShm},
        wl_shm_pool::WlShmPool,
    },
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

/// Output position and size in the compositor's logical coordinate space
/// (the same space Hyprland's `monitors`/`cursorpos` use).
#[derive(Debug, Clone, Copy)]
pub struct Geometry {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl Geometry {
    pub fn x(&self) -> i32 {
        self.x
    }

    pub fn y(&self) -> i32 {
        self.y
    }

    pub fn width(&self) -> i32 {
        self.width
    }

    pub fn height(&self) -> i32 {
        self.height
    }
}

#[derive(Debug, Clone)]
pub struct Output {
    name: String,
    geometry: Geometry,
}

impl Output {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn geometry(&self) -> &Geometry {
        &self.geometry
    }
}

struct OutputState {
    wl_output: WlOutput,
    xdg_output: Option<ZxdgOutputV1>,
    name: Option<String>,
    transform: Transform,
    scale: i32,
    // wl_output.geometry position and current mode, in device pixels.
    x: i32,
    y: i32,
    mode_width: i32,
    mode_height: i32,
    // From xdg-output; authoritative when the compositor provides it.
    logical_x: i32,
    logical_y: i32,
    logical_width: i32,
    logical_height: i32,
    has_logical: bool,
}

impl OutputState {
    fn resolved_name(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("output-{}", self.wl_output.id().protocol_id()))
    }

    fn geometry(&self) -> Geometry {
        if self.has_logical {
            return Geometry {
                x: self.logical_x,
                y: self.logical_y,
                width: self.logical_width,
                height: self.logical_height,
            };
        }

        // No xdg-output: approximate logical size from the mode, the integer
        // scale, and the transform (rotated outputs swap width/height).
        let scale = self.scale.max(1);
        let (mut width, mut height) = (self.mode_width / scale, self.mode_height / scale);
        if matches!(
            self.transform,
            Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270
        ) {
            std::mem::swap(&mut width, &mut height);
        }
        Geometry {
            x: self.x,
            y: self.y,
            width,
            height,
        }
    }
}

#[derive(Default)]
struct State {
    shm: Option<WlShm>,
    screencopy_manager: Option<ZwlrScreencopyManagerV1>,
    xdg_output_manager: Option<ZxdgOutputManagerV1>,
    outputs: Vec<OutputState>,
}

#[derive(Default)]
struct FrameState {
    // (width, height, stride, format) of the first supported buffer layout the
    // compositor advertises.
    params: Option<(u32, u32, u32, ShmFormat)>,
    all_params_received: bool,
    y_invert: bool,
    ready: bool,
    failed: bool,
}

pub struct Screencopy {
    queue: EventQueue<State>,
    state: State,
}

impl Screencopy {
    pub fn new() -> Result<Self, String> {
        let connection = Connection::connect_to_env()
            .map_err(|error| format!("failed to connect to Wayland: {error}"))?;
        let mut queue = connection.new_event_queue();
        connection.display().get_registry(&queue.handle(), ());

        let mut state = State::default();
        // First roundtrip binds the globals (and requests an xdg-output for
        // every wl_output); the second collects the resulting geometry, mode,
        // name and logical-geometry events.
        for _ in 0..2 {
            queue
                .roundtrip(&mut state)
                .map_err(|error| format!("Wayland initialization failed: {error}"))?;
        }

        if state.screencopy_manager.is_none() {
            return Err("compositor does not support zwlr_screencopy_manager_v1".to_string());
        }
        if state.shm.is_none() {
            return Err("compositor does not support wl_shm".to_string());
        }

        Ok(Self { queue, state })
    }

    pub fn outputs(&self) -> Vec<Output> {
        self.state
            .outputs
            .iter()
            .map(|output| Output {
                name: output.resolved_name(),
                geometry: output.geometry(),
            })
            .collect()
    }

    /// Capture one output at its full device-pixel resolution. Returns
    /// `(width, height, rgba_pixels)` in the output's logical orientation
    /// (rotation/flip transforms are undone).
    pub fn capture_output(&mut self, output_name: &str) -> Result<(u32, u32, Vec<u8>), String> {
        let manager = self
            .state
            .screencopy_manager
            .clone()
            .ok_or("compositor does not support zwlr_screencopy_manager_v1")?;
        let shm = self
            .state
            .shm
            .clone()
            .ok_or("compositor does not support wl_shm")?;
        let (wl_output, transform) = self
            .state
            .outputs
            .iter()
            .find(|output| output.resolved_name() == output_name)
            .map(|output| (output.wl_output.clone(), output.transform))
            .ok_or_else(|| format!("output '{output_name}' not found"))?;

        let qh = self.queue.handle();
        let frame_state = Arc::new(Mutex::new(FrameState::default()));
        let frame = manager.capture_output(0, &wl_output, &qh, frame_state.clone());
        // buffer_done (which marks the end of the advertised buffer layouts)
        // only exists since protocol version 3.
        let has_buffer_done = frame.version() >= 3;

        let (width, height, stride, format) = loop {
            {
                let state = lock_frame(&frame_state)?;
                if state.failed {
                    return Err("compositor failed to capture the output".to_string());
                }
                if state.all_params_received || (!has_buffer_done && state.params.is_some()) {
                    match state.params {
                        Some(params) => break params,
                        None => {
                            return Err(
                                "compositor offered no supported screencopy buffer format"
                                    .to_string(),
                            )
                        }
                    }
                }
            }
            self.queue
                .blocking_dispatch(&mut self.state)
                .map_err(|error| format!("Wayland dispatch failed: {error}"))?;
        };

        let size = (stride as u64)
            .checked_mul(height as u64)
            .filter(|size| *size <= i32::MAX as u64)
            .ok_or("screencopy buffer size overflow")? as usize;

        let memfd = rustix::fs::memfd_create("woomer-screencopy", rustix::fs::MemfdFlags::CLOEXEC)
            .map_err(|error| format!("failed to create shared memory buffer: {error}"))?;
        let file = File::from(memfd);
        file.set_len(size as u64)
            .map_err(|error| format!("failed to size shared memory buffer: {error}"))?;

        let pool = shm.create_pool(file.as_fd(), size as i32, &qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            format,
            &qh,
            (),
        );
        frame.copy(&buffer);

        let y_invert = loop {
            {
                let state = lock_frame(&frame_state)?;
                if state.failed {
                    return Err("compositor failed to capture the output".to_string());
                }
                if state.ready {
                    break state.y_invert;
                }
            }
            self.queue
                .blocking_dispatch(&mut self.state)
                .map_err(|error| format!("Wayland dispatch failed: {error}"))?;
        };

        frame.destroy();
        buffer.destroy();
        pool.destroy();

        let mut data = vec![0u8; size];
        (&file)
            .read_exact(&mut data)
            .map_err(|error| format!("failed to read shared memory buffer: {error}"))?;

        // Drop any per-row padding the compositor required.
        let row_bytes = width as usize * 4;
        if stride as usize != row_bytes {
            for row in 1..height as usize {
                data.copy_within(
                    row * stride as usize..row * stride as usize + row_bytes,
                    row * row_bytes,
                );
            }
            data.truncate(row_bytes * height as usize);
        }

        convert_to_rgba(&mut data, format);

        // The buffer is in the output's native (pre-transform) orientation;
        // undo the output transform so the image matches what is on screen.
        let (mut data, width, height) = apply_transform(data, width, height, transform);
        if y_invert {
            flip_vertical_in_place(&mut data, width, height);
        }

        Ok((width, height, data))
    }
}

fn lock_frame(frame_state: &Arc<Mutex<FrameState>>) -> Result<std::sync::MutexGuard<'_, FrameState>, String> {
    frame_state
        .lock()
        .map_err(|_| "screencopy frame state poisoned".to_string())
}

fn is_supported_format(format: ShmFormat) -> bool {
    matches!(
        format,
        ShmFormat::Xrgb8888
            | ShmFormat::Argb8888
            | ShmFormat::Xbgr8888
            | ShmFormat::Abgr8888
            | ShmFormat::Xrgb2101010
            | ShmFormat::Argb2101010
            | ShmFormat::Xbgr2101010
            | ShmFormat::Abgr2101010
    )
}

/// Rewrite 32-bit `wl_shm` pixels as RGBA in place. `wl_shm` formats are
/// little-endian word-defined, so e.g. `Xrgb8888` is B,G,R,x in memory.
fn convert_to_rgba(data: &mut [u8], format: ShmFormat) {
    match format {
        ShmFormat::Xrgb8888 => {
            for pixel in data.chunks_exact_mut(4) {
                pixel.swap(0, 2);
                pixel[3] = 255;
            }
        }
        ShmFormat::Argb8888 => {
            for pixel in data.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
        }
        ShmFormat::Xbgr8888 => {
            for pixel in data.chunks_exact_mut(4) {
                pixel[3] = 255;
            }
        }
        ShmFormat::Abgr8888 => {}
        // 10-bit formats, used when the output scans out a 10-bit buffer.
        ShmFormat::Xrgb2101010
        | ShmFormat::Argb2101010
        | ShmFormat::Xbgr2101010
        | ShmFormat::Abgr2101010 => {
            let bgr_order = matches!(format, ShmFormat::Xrgb2101010 | ShmFormat::Argb2101010);
            let has_alpha = matches!(format, ShmFormat::Argb2101010 | ShmFormat::Abgr2101010);
            for pixel in data.chunks_exact_mut(4) {
                let word = u32::from_le_bytes([pixel[0], pixel[1], pixel[2], pixel[3]]);
                let low = (word & 0x3ff) >> 2;
                let mid = ((word >> 10) & 0x3ff) >> 2;
                let high = ((word >> 20) & 0x3ff) >> 2;
                let (r, b) = if bgr_order { (high, low) } else { (low, high) };
                let alpha = if has_alpha {
                    ((word >> 30) * 85) as u8
                } else {
                    255
                };
                pixel[0] = r as u8;
                pixel[1] = mid as u8;
                pixel[2] = b as u8;
                pixel[3] = alpha;
            }
        }
        _ => {}
    }
}

/// Undo an output transform: map every source pixel to where it appears on
/// screen. 90/270-degree cases swap the image dimensions.
fn apply_transform(data: Vec<u8>, width: u32, height: u32, transform: Transform) -> (Vec<u8>, u32, u32) {
    let (swap_axes, mirror_x, mirror_y) = match transform {
        Transform::Normal => return (data, width, height),
        Transform::_90 => (true, true, false),
        Transform::_180 => (false, true, true),
        Transform::_270 => (true, false, true),
        Transform::Flipped => (false, true, false),
        Transform::Flipped90 => (true, true, true),
        Transform::Flipped180 => (false, false, true),
        Transform::Flipped270 => (true, false, false),
        _ => return (data, width, height),
    };

    let (new_width, new_height) = if swap_axes { (height, width) } else { (width, height) };
    let mut out = vec![0u8; data.len()];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let (mut dst_x, mut dst_y) = if swap_axes { (y, x) } else { (x, y) };
            if mirror_x {
                dst_x = new_width as usize - 1 - dst_x;
            }
            if mirror_y {
                dst_y = new_height as usize - 1 - dst_y;
            }
            let src = (y * width as usize + x) * 4;
            let dst = (dst_y * new_width as usize + dst_x) * 4;
            out[dst..dst + 4].copy_from_slice(&data[src..src + 4]);
        }
    }
    (out, new_width, new_height)
}

fn flip_vertical_in_place(data: &mut [u8], width: u32, height: u32) {
    let stride = width as usize * 4;
    for row in 0..height as usize / 2 {
        let (top, bottom) = data.split_at_mut((height as usize - 1 - row) * stride);
        top[row * stride..row * stride + stride].swap_with_slice(&mut bottom[..stride]);
    }
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };

        match interface.as_str() {
            "wl_shm" => {
                state.shm = Some(registry.bind::<WlShm, _, _>(name, 1, qh, ()));
            }
            "zwlr_screencopy_manager_v1" => {
                state.screencopy_manager = Some(registry.bind::<ZwlrScreencopyManagerV1, _, _>(
                    name,
                    version.min(3),
                    qh,
                    (),
                ));
            }
            "zxdg_output_manager_v1" => {
                let manager =
                    registry.bind::<ZxdgOutputManagerV1, _, _>(name, version.min(3), qh, ());
                for output in &mut state.outputs {
                    if output.xdg_output.is_none() {
                        output.xdg_output = Some(manager.get_xdg_output(&output.wl_output, qh, ()));
                    }
                }
                state.xdg_output_manager = Some(manager);
            }
            "wl_output" => {
                let wl_output = registry.bind::<WlOutput, _, _>(name, version.min(4), qh, ());
                let xdg_output = state
                    .xdg_output_manager
                    .as_ref()
                    .map(|manager| manager.get_xdg_output(&wl_output, qh, ()));
                state.outputs.push(OutputState {
                    wl_output,
                    xdg_output,
                    name: None,
                    transform: Transform::Normal,
                    scale: 1,
                    x: 0,
                    y: 0,
                    mode_width: 0,
                    mode_height: 0,
                    logical_x: 0,
                    logical_y: 0,
                    logical_width: 0,
                    logical_height: 0,
                    has_logical: false,
                });
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(entry) = state
            .outputs
            .iter_mut()
            .find(|entry| entry.wl_output.id() == output.id())
        else {
            return;
        };

        match event {
            wl_output::Event::Geometry {
                x, y, transform, ..
            } => {
                entry.x = x;
                entry.y = y;
                if let WEnum::Value(transform) = transform {
                    entry.transform = transform;
                }
            }
            wl_output::Event::Mode { width, height, .. } => {
                entry.mode_width = width;
                entry.mode_height = height;
            }
            wl_output::Event::Scale { factor } => {
                entry.scale = factor;
            }
            wl_output::Event::Name { name } => {
                entry.name = Some(name);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, ()> for State {
    fn event(
        state: &mut Self,
        xdg_output: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(entry) = state.outputs.iter_mut().find(|entry| {
            entry
                .xdg_output
                .as_ref()
                .is_some_and(|candidate| candidate.id() == xdg_output.id())
        }) else {
            return;
        };

        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                entry.logical_x = x;
                entry.logical_y = y;
                entry.has_logical = true;
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                entry.logical_width = width;
                entry.logical_height = height;
                entry.has_logical = true;
            }
            zxdg_output_v1::Event::Name { name } => {
                // Prefer the wl_output v4 name; older compositors only send this one.
                entry.name.get_or_insert(name);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, Arc<Mutex<FrameState>>> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        frame_state: &Arc<Mutex<FrameState>>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Ok(mut state) = frame_state.lock() else {
            return;
        };

        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } if state.params.is_none() && is_supported_format(format) => {
                state.params = Some((width, height, stride, format));
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => {
                state.all_params_received = true;
            }
            zwlr_screencopy_frame_v1::Event::Flags {
                flags: WEnum::Value(flags),
            } => {
                state.y_invert = flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                state.ready = true;
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                state.failed = true;
            }
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(State: ignore WlShm);
wayland_client::delegate_noop!(State: ignore WlShmPool);
wayland_client::delegate_noop!(State: ignore WlBuffer);
wayland_client::delegate_noop!(State: ignore ZwlrScreencopyManagerV1);
wayland_client::delegate_noop!(State: ignore ZxdgOutputManagerV1);
