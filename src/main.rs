mod capture;

use std::{env, ffi::CStr, process, thread};

use capture::{Output, Screencopy};
use hyprland::{
    data::{CursorPosition, Monitor, Monitors},
    prelude::*,
};
use raylib::{ffi, ffi::SetWindowMonitor, prelude::*};

const SPOTLIGHT_TINT: Color = Color::new(0x00, 0x00, 0x00, 190);
const SPOTLIGHT_UNIT_RADIUS: f32 = 100.0;
const MAX_FRAME_TIME: f32 = 1.0 / 15.0;
const ANIMATION_EPSILON: f32 = 0.001;
const STROKE_COLOR: Color = Color::new(230, 41, 55, 255);
// Logical pixels; multiplied by the output scale into device pixels.
const STROKE_BASE_THICKNESS: f32 = 4.0;
const STROKE_FADE_SECONDS: f64 = 3.0;
// Skip points closer than this (device pixels) to keep strokes sparse.
const STROKE_MIN_SEGMENT_LENGTH: f32 = 2.0;
const POINTER_DOT_RADIUS_FACTOR: f32 = 2.5;
#[cfg(feature = "dev")]
const DEV_SPOTLIGHT_SHADER_PATH: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/shaders/spotlight.fs");

/// A freehand annotation drawn with the middle mouse button. Points live in
/// image-space (the camera's world coordinates), so strokes pan and zoom with
/// the content. A single-point stroke renders as a pointer dot.
struct Stroke {
    points: Vec<Vector2>,
    released_at: Option<f64>,
}

/// 1.0 while the stroke is being drawn, then a linear fade to 0.0 over
/// `STROKE_FADE_SECONDS` after release.
fn stroke_alpha(now: f64, released_at: Option<f64>) -> f32 {
    match released_at {
        None => 1.0,
        Some(released) => (1.0 - (now - released) / STROKE_FADE_SECONDS).clamp(0.0, 1.0) as f32,
    }
}

#[derive(Clone, Copy)]
struct SpotlightUniforms {
    tint: i32,
    cursor_position: i32,
    radius_squared: i32,
}

fn decay_factor(rate: f32, frame_time: f32) -> f32 {
    (-rate * frame_time).exp()
}

fn decay_displacement(value: f32, rate: f32, frame_time: f32) -> f32 {
    value * (1.0 - decay_factor(rate, frame_time)) / rate
}

fn approach_exponentially(current: f32, target: f32, rate: f32, frame_time: f32) -> f32 {
    target + (current - target) * decay_factor(rate, frame_time)
}

fn apply_scroll(
    amount: f32,
    spotlight_enabled: bool,
    shift_down: bool,
    zoom_delta: &mut f32,
    radius_delta: &mut f32,
) {
    if amount == 0.0 {
        return;
    }

    if spotlight_enabled && shift_down {
        *radius_delta -= amount;
    } else {
        *zoom_delta += amount;
    }
}

fn validate_rgba_buffer(data_len: usize, width: u32, height: u32) -> Result<(i32, i32), String> {
    let width_usize = usize::try_from(width).map_err(|_| "capture width is too large")?;
    let height_usize = usize::try_from(height).map_err(|_| "capture height is too large")?;
    let expected_len = width_usize
        .checked_mul(height_usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("capture dimensions overflow")?;

    if data_len != expected_len {
        return Err(format!(
            "invalid RGBA capture length: got {data_len}, expected {expected_len}"
        ));
    }

    let width = i32::try_from(width).map_err(|_| "capture width exceeds raylib limits")?;
    let height = i32::try_from(height).map_err(|_| "capture height exceeds raylib limits")?;
    Ok((width, height))
}

fn load_rgba_texture(data: &mut [u8], width: u32, height: u32) -> Result<Texture2D, String> {
    let (width, height) = validate_rgba_buffer(data.len(), width, height)?;
    let image = ffi::Image {
        data: data.as_mut_ptr().cast(),
        width,
        height,
        mipmaps: 1,
        format: PixelFormat::PIXELFORMAT_UNCOMPRESSED_R8G8B8A8 as i32,
    };

    // LoadTextureFromImage uploads synchronously and does not take ownership of image.data.
    let texture = unsafe { ffi::LoadTextureFromImage(image) };
    if texture.id == 0 {
        return Err("raylib failed to upload screenshot texture".to_string());
    }

    Ok(unsafe { Texture2D::from_raw(texture) })
}

fn configure_spotlight_shader(
    shader: &mut Shader,
    framebuffer_size: Vector2,
) -> Result<SpotlightUniforms, String> {
    if !shader.is_shader_valid() {
        return Err("spotlight shader failed to compile or link".to_string());
    }

    let uniforms = SpotlightUniforms {
        tint: shader.get_shader_location("spotlightTint"),
        cursor_position: shader.get_shader_location("cursorPosition"),
        radius_squared: shader.get_shader_location("spotlightRadiusSquared"),
    };

    for (name, location) in [
        ("spotlightTint", uniforms.tint),
        ("cursorPosition", uniforms.cursor_position),
        ("spotlightRadiusSquared", uniforms.radius_squared),
    ] {
        if location < 0 {
            return Err(format!("spotlight shader is missing uniform {name}"));
        }
    }

    let texture_size = shader.get_shader_location("textureSize");
    if texture_size < 0 {
        return Err("spotlight shader is missing uniform textureSize".to_string());
    }
    shader.set_shader_value(texture_size, framebuffer_size);

    Ok(uniforms)
}

/// Look up the fractional scale Hyprland applies to the given output.
///
/// A fullscreen surface is sized in *logical* pixels (e.g. 1280x720 at scale
/// 1.5) but is backed by a device-pixel GL buffer at the monitor's native
/// resolution (logical * scale = 1920x1080). On this GLFW/Wayland build raylib
/// renders 1:1 into device pixels (a logical-unit coordinate lands on the device
/// pixel of the same number) but leaves its GL viewport at the *logical* size,
/// so our scene only covers the bottom-left ~2/3 of the buffer. We use this
/// scale to widen the viewport to the real device buffer and to size the
/// geometry and shader uniforms in device pixels so everything stays aligned.
/// (`get_mouse_position` already reports device pixels on this build, so live
/// cursor reads are used as-is; only Hyprland's logical `cursorpos` is scaled.)
/// (`FLAG_WINDOW_HIGHDPI` would normally surface the device size but is broken
/// here — it reports a garbage framebuffer.)
fn output_scale(output_name: &str, monitors: Option<&[Monitor]>) -> f32 {
    monitors
        .and_then(|monitors| {
            monitors
                .iter()
                .find(|m| m.name == output_name)
                .map(|m| m.scale)
        })
        .filter(|s| *s > 0.0)
        .unwrap_or_else(|| {
            eprintln!("warning: could not determine scale for {output_name}; assuming scale 1.0");
            1.0
        })
}

/// Pick the output the user is actually working on, used when no `--monitor` is
/// given. Wayland enumeration order is arbitrary, so `outputs[0]` is often an
/// idle monitor; prefer Hyprland's focused monitor (matched by name to dodge any
/// logical-vs-device coordinate mismatch between the two libraries), fall back to
/// the output containing the cursor, then to the first enumerated output.
fn active_output<'a>(
    outputs: &'a [Output],
    monitors: Option<&[Monitor]>,
    cursor_position: Option<&CursorPosition>,
) -> &'a Output {
    if let Some(monitors) = monitors {
        if let Some(name) = monitors.iter().find(|m| m.focused).map(|m| &m.name) {
            if let Some(out) = outputs.iter().find(|o| o.name() == name) {
                return out;
            }
        }
    }

    if let Some(pos) = cursor_position {
        let (x, y) = (pos.x as i32, pos.y as i32);
        if let Some(out) = outputs.iter().find(|o| {
            let g = o.geometry();
            x >= g.x() && x < g.x() + g.width() && y >= g.y() && y < g.y() + g.height()
        }) {
            return out;
        }
    }

    &outputs[0]
}

fn get_initial_cursor_pos_for_output(
    cursor_position: Option<&CursorPosition>,
    out_x: i32,
    out_y: i32,
    out_w: i32,
    out_h: i32,
) -> Option<Vector2> {
    let pos = cursor_position?;

    let local_x = pos.x as f32 - out_x as f32;
    let local_y = pos.y as f32 - out_y as f32;

    Some(Vector2::new(
        local_x.clamp(0.0, out_w as f32),
        local_y.clamp(0.0, out_h as f32),
    ))
}

// GLFW loads libdecor (which drags in its GTK plugin) on Wayland even for
// undecorated windows; this init hint disables it. Not exposed by raylib, but
// GLFW is linked as a shared library so the symbol is reachable directly.
const GLFW_WAYLAND_LIBDECOR: i32 = 0x0005_3001;
const GLFW_WAYLAND_DISABLE_LIBDECOR: i32 = 0x0003_8002;
extern "C" {
    fn glfwInitHint(hint: i32, value: i32);
}

// No-op shims for GLFW's joystick API. raylib force-initializes the joystick
// subsystem inside InitWindow, making GLFW open and close every
// /dev/input/event* device before the window appears — 10-25ms per device
// (~250ms total) on this machine. Gamepads are useless for a screen zoomer, so
// these definitions shadow the shared-library symbols at link time (definitions
// in the executable win over libglfw.so) and raylib's calls become free. These
// five are the only joystick entry points raylib references; the state-reading
// ones are all gated on glfwJoystickPresent returning true, so returning
// "no joysticks" keeps every code path consistent.
#[no_mangle]
pub extern "C" fn glfwSetJoystickCallback(
    _callback: Option<extern "C" fn(i32, i32)>,
) -> Option<extern "C" fn(i32, i32)> {
    None
}
#[no_mangle]
pub extern "C" fn glfwJoystickPresent(_joystick_id: i32) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn glfwGetJoystickName(_joystick_id: i32) -> *const std::ffi::c_char {
    std::ptr::null()
}
#[no_mangle]
pub extern "C" fn glfwGetGamepadState(_joystick_id: i32, _state: *mut std::ffi::c_void) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn glfwUpdateGamepadMappings(_mappings: *const std::ffi::c_char) -> i32 {
    0
}

fn raylib_monitor_index(selected_output: &Output, outputs: &[Output]) -> Option<i32> {
    let monitor_count = unsafe { ffi::GetMonitorCount() };

    for index in 0..monitor_count {
        let name = unsafe { ffi::GetMonitorName(index) };
        if !name.is_null()
            && unsafe { CStr::from_ptr(name) }.to_string_lossy() == selected_output.name()
        {
            return Some(index);
        }
    }

    let geometry = selected_output.geometry();
    for index in 0..monitor_count {
        let position = unsafe { ffi::GetMonitorPosition(index) };
        if position.x.round() as i32 == geometry.x() && position.y.round() as i32 == geometry.y() {
            return Some(index);
        }
    }

    let fallback = outputs
        .iter()
        .position(|output| output.name() == selected_output.name())?;
    (fallback < monitor_count as usize).then_some(fallback as i32)
}

fn main() {
    let mut args = env::args();
    let bin = args.next().unwrap();

    let mut monitor_name: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--monitor" => {
                monitor_name = args.next().or_else(|| {
                    eprintln!("--monitor needs a value");
                    process::exit(1);
                })
            }
            "--help" | "-h" => print_help_and_exit(&bin, 0),
            _ => {
                eprintln!("unknown argument: {arg}");
                print_help_and_exit(&bin, 1);
            }
        }
    }

    let mut screencopy = Screencopy::new().expect("failed to initialize screen capture");
    let outputs = screencopy.outputs();

    if outputs.is_empty() {
        eprintln!("No Wayland outputs found.");
        process::exit(1);
    }

    let hyprland_monitors = Monitors::get().ok().map(|monitors| monitors.to_vec());
    let initial_cursor_position = CursorPosition::get().ok();

    let selected_output = match monitor_name {
        None => active_output(
            &outputs,
            hyprland_monitors.as_deref(),
            initial_cursor_position.as_ref(),
        ),
        Some(ref name) => outputs
            .iter()
            .find(|out| out.name() == name)
            .unwrap_or_else(|| {
                eprintln!("Output '{}' not found.", name);
                process::exit(1);
            }),
    };

    let spotlight_mouse_position_logical = get_initial_cursor_pos_for_output(
        initial_cursor_position.as_ref(),
        selected_output.geometry().x(),
        selected_output.geometry().y(),
        selected_output.geometry().width(),
        selected_output.geometry().height(),
    )
    .unwrap_or(Vector2::new(
        selected_output.geometry().width() as f32 * 0.5,
        selected_output.geometry().height() as f32 * 0.5,
    ));

    // Capture on a worker thread so the (tens of ms) screencopy roundtrip
    // overlaps raylib's much slower window/GL init below. This cannot capture
    // woomer's own window: the surface is only mapped on the first buffer swap,
    // which happens after the capture is joined and drawn.
    let capture_output_name = selected_output.name().to_string();
    let capture_thread = thread::spawn(move || {
        screencopy
            .capture_output(&capture_output_name)
            .expect("failed to capture output")
    });

    // Hyprland always sizes a fullscreen surface to the output's *logical* size
    // (e.g. 1280x720 for a 1920x1080 panel at scale 1.5), so that is the size we
    // request the window at. The real GL buffer behind it is the device size
    // (logical * scale); we render the whole scene in device pixels and widen the
    // GL viewport to match each frame (see `output_scale`).
    let logical_width = selected_output.geometry().width();
    let logical_height = selected_output.geometry().height();

    // Real device-pixel framebuffer backing the fullscreen surface.
    let scale = output_scale(selected_output.name(), hyprland_monitors.as_deref());
    let fb_width = (logical_width as f32 * scale).round() as i32;
    let fb_height = (logical_height as f32 * scale).round() as i32;

    // Everything downstream works in device pixels (see `output_scale`).
    // `get_mouse_position` already reports device pixels on this GLFW/Wayland
    // build, so live cursor reads need no conversion; only this initial position,
    // which comes from Hyprland's *logical* `cursorpos`, must be lifted to device.
    let mut spotlight_mouse_position = spotlight_mouse_position_logical * scale;

    // Must run before raylib calls glfwInit.
    unsafe { glfwInitHint(GLFW_WAYLAND_LIBDECOR, GLFW_WAYLAND_DISABLE_LIBDECOR) };

    let (mut rl, thread) = raylib::init()
        .title(env!("CARGO_BIN_NAME"))
        .size(logical_width, logical_height)
        .transparent()
        .undecorated()
        .fullscreen()
        .vsync()
        .build();

    let monitor_index = raylib_monitor_index(selected_output, &outputs).unwrap_or_else(|| {
        eprintln!(
            "Could not map Wayland output '{}' to a raylib monitor.",
            selected_output.name()
        );
        process::exit(1);
    });

    unsafe {
        SetWindowMonitor(monitor_index);
    }

    // `grim` captures at the output's full device pixel resolution, which on a
    // fractionally-scaled output is even larger than the monitor's mode (e.g.
    // 2880x1620 for a 1920x1080 panel at scale 1.5). We keep this full-res
    // texture for crisp zooming and just draw it scaled to the logical window.
    let (capture_width, capture_height, mut raw_pixels) = capture_thread
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic));

    let screenshot_texture = load_rgba_texture(&mut raw_pixels, capture_width, capture_height)
        .unwrap_or_else(|error| {
            eprintln!("failed to load screenshot into a texture: {error}");
            process::exit(1);
        });
    drop(raw_pixels);

    // Draw the full-resolution capture across the entire device framebuffer so
    // it fills the screen and stays sharp.
    let texture_src = Rectangle::new(0.0, 0.0, capture_width as f32, capture_height as f32);
    let texture_dst = Rectangle::new(0.0, 0.0, fb_width as f32, fb_height as f32);

    #[cfg(feature = "dev")]
    let mut spotlight_shader = rl.load_shader(&thread, None, Some(DEV_SPOTLIGHT_SHADER_PATH));

    #[cfg(not(feature = "dev"))]
    let mut spotlight_shader =
        rl.load_shader_from_memory(&thread, None, Some(include_str!("../shaders/spotlight.fs")));

    let mut rl_camera = Camera2D {
        zoom: 1.0,
        ..Camera2D::default()
    };

    let mut delta_scale = 0.0f32;
    let mut scale_pivot = rl.get_mouse_position();
    let mut velocity = Vector2::default();
    let mut spotlight_radius_multiplier = 1.0;
    let mut spotlight_radius_multiplier_delta = 0.0f32;
    let mut spotlight_opacity = 0.0f32;
    let mut strokes: Vec<Stroke> = Vec::new();
    let stroke_thickness_device = STROKE_BASE_THICKNESS * scale;

    let spotlight_uniforms = configure_spotlight_shader(
        &mut spotlight_shader,
        Vector2::new(fb_width as f32, fb_height as f32),
    )
    .unwrap_or_else(|error| {
        eprintln!("{error}");
        process::exit(1);
    });
    #[cfg(feature = "dev")]
    let mut spotlight_uniforms = spotlight_uniforms;

    // Draw one fully-populated frame immediately so the first visible frame
    // is the screenshot, not an empty/unstyled window.
    {
        let mut d = rl.begin_drawing(&thread);
        let mut mode2d = d.begin_mode2D(rl_camera);
        // raylib leaves the viewport at the logical size; widen it to the real
        // device buffer so the scene fills the whole screen.
        unsafe { ffi::rlViewport(0, 0, fb_width, fb_height) };
        mode2d.clear_background(Color::get_color(0));
        mode2d.draw_texture_pro(
            &screenshot_texture,
            texture_src,
            texture_dst,
            Vector2::zero(),
            0.0,
            Color::WHITE,
        );
    }

    let mut should_exit = false;
    while !rl.window_should_close() && !should_exit {
        if rl.is_key_pressed(KeyboardKey::KEY_Q) || rl.is_key_pressed(KeyboardKey::KEY_A) {
            should_exit = true;
        }

        if should_exit {
            break;
        }

        if rl.is_mouse_button_down(MouseButton::MOUSE_BUTTON_RIGHT) {
            break;
        }

        #[cfg(feature = "dev")]
        if rl.is_key_pressed(KeyboardKey::KEY_R) {
            let mut reloaded_shader =
                rl.load_shader(&thread, None, Some(DEV_SPOTLIGHT_SHADER_PATH));
            match configure_spotlight_shader(
                &mut reloaded_shader,
                Vector2::new(fb_width as f32, fb_height as f32),
            ) {
                Ok(uniforms) => {
                    spotlight_shader = reloaded_shader;
                    spotlight_uniforms = uniforms;
                }
                Err(error) => eprintln!("shader reload failed: {error}"),
            }
        }

        let enable_spotlight = rl.is_key_down(KeyboardKey::KEY_LEFT_CONTROL)
            || rl.is_key_down(KeyboardKey::KEY_RIGHT_CONTROL);
        let shift_down = rl.is_key_down(KeyboardKey::KEY_LEFT_SHIFT)
            || rl.is_key_down(KeyboardKey::KEY_RIGHT_SHIFT);

        let scrolled_amount = rl.get_mouse_wheel_move_v().y;
        let frame_time = rl.get_frame_time().clamp(0.0, MAX_FRAME_TIME);
        let mouse_position = rl.get_mouse_position();

        let target_opacity = if enable_spotlight {
            SPOTLIGHT_TINT.a as f32 / 255.0
        } else {
            0.0
        };

        spotlight_opacity =
            approach_exponentially(spotlight_opacity, target_opacity, 4.0, frame_time);
        if (target_opacity - spotlight_opacity).abs() < ANIMATION_EPSILON {
            spotlight_opacity = target_opacity;
        }

        if rl.is_key_pressed(KeyboardKey::KEY_LEFT_CONTROL)
            || rl.is_key_pressed(KeyboardKey::KEY_RIGHT_CONTROL)
        {
            spotlight_radius_multiplier = 5.0;
            spotlight_radius_multiplier_delta = -15.0;
        }

        apply_scroll(
            scrolled_amount,
            enable_spotlight,
            shift_down,
            &mut delta_scale,
            &mut spotlight_radius_multiplier_delta,
        );
        if scrolled_amount != 0.0 {
            scale_pivot = mouse_position;
        }

        if delta_scale.abs() > 0.5 {
            let p0 = scale_pivot / rl_camera.zoom;
            rl_camera.zoom = (rl_camera.zoom + decay_displacement(delta_scale, 4.0, frame_time))
                .clamp(1.0, 10.0);
            let p1 = scale_pivot / rl_camera.zoom;
            rl_camera.target += p0 - p1;
            delta_scale *= decay_factor(4.0, frame_time);
        } else {
            delta_scale = 0.0;
        }

        spotlight_radius_multiplier = (spotlight_radius_multiplier
            + decay_displacement(spotlight_radius_multiplier_delta, 4.0, frame_time))
        .clamp(0.3, 10.0);

        spotlight_radius_multiplier_delta *= decay_factor(4.0, frame_time);
        if spotlight_radius_multiplier_delta.abs() < ANIMATION_EPSILON {
            spotlight_radius_multiplier_delta = 0.0;
        }

        const VELOCITY_THRESHOLD: f32 = 15.0;
        let dragging = rl.is_mouse_button_down(MouseButton::MOUSE_BUTTON_LEFT);
        if dragging {
            let prev_mouse = mouse_position - rl.get_mouse_delta();
            let delta = rl.get_screen_to_world2D(prev_mouse, rl_camera)
                - rl.get_screen_to_world2D(mouse_position, rl_camera);

            rl_camera.target += delta;
            velocity = delta / frame_time.max(f32::EPSILON);
        } else if velocity.length_sqr() > VELOCITY_THRESHOLD * VELOCITY_THRESHOLD {
            rl_camera.target += velocity * ((1.0 - decay_factor(6.0, frame_time)) / 6.0);
            velocity *= decay_factor(6.0, frame_time);
        } else {
            velocity = Vector2::zero();
        }

        if rl.is_cursor_on_screen() {
            spotlight_mouse_position = rl.get_screen_to_world2D(mouse_position, rl_camera);
        }

        let now = rl.get_time();
        let world_mouse = spotlight_mouse_position;
        if rl.is_mouse_button_pressed(MouseButton::MOUSE_BUTTON_MIDDLE) {
            strokes.push(Stroke {
                points: vec![world_mouse],
                released_at: None,
            });
        } else if rl.is_mouse_button_down(MouseButton::MOUSE_BUTTON_MIDDLE) {
            if let Some(stroke) = strokes.last_mut().filter(|s| s.released_at.is_none()) {
                let far_enough = stroke.points.last().is_none_or(|last| {
                    (*last - world_mouse).length_sqr()
                        >= STROKE_MIN_SEGMENT_LENGTH * STROKE_MIN_SEGMENT_LENGTH
                });
                if far_enough {
                    stroke.points.push(world_mouse);
                }
            }
        } else if rl.is_mouse_button_released(MouseButton::MOUSE_BUTTON_MIDDLE) {
            if let Some(stroke) = strokes.last_mut().filter(|s| s.released_at.is_none()) {
                stroke.released_at = Some(now);
            }
        }
        strokes.retain(|s| stroke_alpha(now, s.released_at) > 0.0);

        let animation_active = dragging
            || delta_scale != 0.0
            || spotlight_radius_multiplier_delta != 0.0
            || velocity != Vector2::zero()
            || (target_opacity - spotlight_opacity).abs() >= ANIMATION_EPSILON
            // Fading strokes need continuous frames, not event-driven ones.
            || !strokes.is_empty();
        unsafe {
            if animation_active {
                ffi::DisableEventWaiting();
            } else {
                ffi::EnableEventWaiting();
            }
        }

        let mut d = rl.begin_drawing(&thread);
        let mut mode2d = d.begin_mode2D(rl_camera);
        // raylib leaves the viewport at the logical size; widen it to the real
        // device buffer so the scene fills the whole screen.
        unsafe { ffi::rlViewport(0, 0, fb_width, fb_height) };
        mode2d.clear_background(Color::get_color(0));

        if enable_spotlight || spotlight_opacity > 0.001 {
            let mouse_position = spotlight_mouse_position;

            spotlight_shader.set_shader_value(
                spotlight_uniforms.tint,
                Vector4::new(
                    SPOTLIGHT_TINT.r as f32 / 255.0,
                    SPOTLIGHT_TINT.g as f32 / 255.0,
                    SPOTLIGHT_TINT.b as f32 / 255.0,
                    spotlight_opacity,
                ),
            );

            spotlight_shader.set_shader_value(
                spotlight_uniforms.cursor_position,
                Vector2::new(mouse_position.x, mouse_position.y),
            );

            let spotlight_radius =
                SPOTLIGHT_UNIT_RADIUS * spotlight_radius_multiplier / rl_camera.zoom;
            spotlight_shader.set_shader_value(
                spotlight_uniforms.radius_squared,
                spotlight_radius * spotlight_radius,
            );

            let mut shader_mode = mode2d.begin_shader_mode(&mut spotlight_shader);
            shader_mode.draw_texture_pro(
                &screenshot_texture,
                texture_src,
                texture_dst,
                Vector2::zero(),
                0.0,
                Color::WHITE,
            );
        } else {
            mode2d.draw_texture_pro(
                &screenshot_texture,
                texture_src,
                texture_dst,
                Vector2::zero(),
                0.0,
                Color::WHITE,
            );
        }

        // Constant on-screen thickness regardless of zoom.
        let thickness = stroke_thickness_device / rl_camera.zoom;
        for stroke in &strokes {
            let color = STROKE_COLOR.alpha(stroke_alpha(now, stroke.released_at));
            if let [point] = stroke.points.as_slice() {
                mode2d.draw_circle_v(*point, thickness * POINTER_DOT_RADIUS_FACTOR, color);
                continue;
            }
            for pair in stroke.points.windows(2) {
                mode2d.draw_line_ex(pair[0], pair[1], thickness, color);
            }
            // Round joins and caps.
            for point in &stroke.points {
                mode2d.draw_circle_v(*point, thickness * 0.5, color);
            }
        }
    }
}

fn print_help_and_exit(bin: &str, exit_code: i32) -> ! {
    eprintln!(
        "\
{bin}  – Wayland screen-zoom tool

USAGE:
    {bin} [--monitor <name>]

OPTIONS:
    --monitor <name>   Target monitor (Wayland output name); defaults to the focused output.
    -h, --help         Print this help.",
        bin = bin
    );
    process::exit(exit_code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exponential_approach_does_not_overshoot() {
        let result = approach_exponentially(0.0, 1.0, 4.0, 1.0);
        assert!((0.0..1.0).contains(&result));
    }

    #[test]
    fn decay_never_reverses_value() {
        let result = 100.0 * decay_factor(6.0, 1.0);
        assert!((0.0..100.0).contains(&result));
    }

    #[test]
    fn decay_displacement_is_frame_rate_independent() {
        fn simulate(frame_time: f32, frames: usize) -> f32 {
            let mut value = 100.0;
            let mut displacement = 0.0;
            for _ in 0..frames {
                displacement += decay_displacement(value, 6.0, frame_time);
                value *= decay_factor(6.0, frame_time);
            }
            displacement
        }

        let at_15_fps = simulate(1.0 / 15.0, 15);
        let at_60_fps = simulate(1.0 / 60.0, 60);
        assert!((at_15_fps - at_60_fps).abs() < 0.0001);
    }

    #[test]
    fn shift_wheel_still_zooms_without_spotlight() {
        let mut zoom_delta = 0.0;
        let mut radius_delta = 0.0;
        apply_scroll(1.0, false, true, &mut zoom_delta, &mut radius_delta);
        assert_eq!(zoom_delta, 1.0);
        assert_eq!(radius_delta, 0.0);
    }

    #[test]
    fn control_shift_wheel_changes_spotlight_radius() {
        let mut zoom_delta = 0.0;
        let mut radius_delta = 0.0;
        apply_scroll(1.0, true, true, &mut zoom_delta, &mut radius_delta);
        assert_eq!(zoom_delta, 0.0);
        assert_eq!(radius_delta, -1.0);
    }

    #[test]
    fn stroke_is_opaque_while_being_drawn() {
        assert_eq!(stroke_alpha(1000.0, None), 1.0);
    }

    #[test]
    fn stroke_fades_linearly_after_release() {
        assert_eq!(stroke_alpha(10.0, Some(10.0)), 1.0);
        let midway = stroke_alpha(10.0 + STROKE_FADE_SECONDS / 2.0, Some(10.0));
        assert!((midway - 0.5).abs() < 0.0001);
        assert_eq!(stroke_alpha(10.0 + STROKE_FADE_SECONDS, Some(10.0)), 0.0);
        assert_eq!(stroke_alpha(10.0 + STROKE_FADE_SECONDS * 2.0, Some(10.0)), 0.0);
    }

    #[test]
    fn rejects_invalid_rgba_buffer_length() {
        assert!(validate_rgba_buffer(15, 2, 2).is_err());
        assert_eq!(validate_rgba_buffer(16, 2, 2), Ok((2, 2)));
    }

    #[cfg(feature = "dev")]
    #[test]
    fn dev_shader_path_is_independent_of_working_directory() {
        let path = std::path::Path::new(DEV_SPOTLIGHT_SHADER_PATH);
        assert!(path.is_absolute());
        assert!(path.is_file());
    }
}
