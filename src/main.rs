use std::{env, process};

use grim_rs::Grim;
use hyprland::{
    data::{CursorPosition, Monitors},
    prelude::*,
};
use raylib::{
    ffi,
    ffi::{Image as FfiImage, SetWindowMonitor},
    prelude::*,
};

const SPOTLIGHT_TINT: Color = Color::new(0x00, 0x00, 0x00, 190);

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
fn output_scale(output_name: &str) -> f32 {
    Monitors::get()
        .ok()
        .and_then(|monitors| {
            monitors
                .into_iter()
                .find(|m| m.name == output_name)
                .map(|m| m.scale)
        })
        .filter(|s| *s > 0.0)
        .unwrap_or(1.0)
}

fn get_initial_cursor_pos_for_output(
    out_x: i32,
    out_y: i32,
    out_w: i32,
    out_h: i32,
) -> Option<Vector2> {
    let pos = CursorPosition::get().ok()?;

    let local_x = pos.x as f32 - out_x as f32;
    let local_y = pos.y as f32 - out_y as f32;

    Some(Vector2::new(
        local_x.clamp(0.0, out_w as f32),
        local_y.clamp(0.0, out_h as f32),
    ))
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
            _ => print_help_and_exit(&bin),
        }
    }

    let mut grim = Grim::new().expect("failed to initialize grim-rs");
    let outputs = grim.get_outputs().expect("failed to get outputs");

    if outputs.is_empty() {
        eprintln!("No Wayland outputs found.");
        process::exit(1);
    }

    let selected_output = match monitor_name {
        None => &outputs[0],
        Some(ref name) => outputs
            .iter()
            .find(|out| out.name() == name)
            .unwrap_or_else(|| {
                eprintln!("Output '{}' not found.", name);
                process::exit(1);
            }),
    };

    let spotlight_mouse_position_logical = get_initial_cursor_pos_for_output(
        selected_output.geometry().x(),
        selected_output.geometry().y(),
        selected_output.geometry().width() as i32,
        selected_output.geometry().height() as i32,
    )
    .unwrap_or(Vector2::new(
        selected_output.geometry().width() as f32 * 0.5,
        selected_output.geometry().height() as f32 * 0.5,
    ));

    let screenshot = grim
        .capture_output(selected_output.name())
        .expect("failed to capture output");
    // `grim` captures at the output's full device pixel resolution, which on a
    // fractionally-scaled output is even larger than the monitor's mode (e.g.
    // 2880x1620 for a 1920x1080 panel at scale 1.5). We keep this full-res
    // texture for crisp zooming and just draw it scaled to the logical window.
    let capture_width = screenshot.width();
    let capture_height = screenshot.height();
    let raw_pixels = screenshot.into_data();

    // Hyprland always sizes a fullscreen surface to the output's *logical* size
    // (e.g. 1280x720 for a 1920x1080 panel at scale 1.5), so that is the size we
    // request the window at. The real GL buffer behind it is the device size
    // (logical * scale); we render the whole scene in device pixels and widen the
    // GL viewport to match each frame (see `output_scale`).
    let logical_width = selected_output.geometry().width() as i32;
    let logical_height = selected_output.geometry().height() as i32;

    // Real device-pixel framebuffer backing the fullscreen surface.
    let scale = output_scale(selected_output.name());
    let fb_width = (logical_width as f32 * scale).round() as i32;
    let fb_height = (logical_height as f32 * scale).round() as i32;

    // Everything downstream works in device pixels (see `output_scale`).
    // `get_mouse_position` already reports device pixels on this GLFW/Wayland
    // build, so live cursor reads need no conversion; only this initial position,
    // which comes from Hyprland's *logical* `cursorpos`, must be lifted to device.
    let mut spotlight_mouse_position = spotlight_mouse_position_logical * scale;

    let (mut rl, thread) = raylib::init()
        .title(env!("CARGO_BIN_NAME"))
        .size(logical_width, logical_height)
        .transparent()
        .undecorated()
        .fullscreen()
        .vsync()
        .build();

    let idx = outputs
        .iter()
        .position(|o| o.name() == selected_output.name())
        .expect("Monitor not found");

    unsafe {
        SetWindowMonitor(idx as i32);
    }

    let screenshot_image = unsafe {
        Image::from_raw(FfiImage {
            // raylib frees this memory for us
            data: Box::new(raw_pixels).leak().as_mut_ptr().cast(),
            format: PixelFormat::PIXELFORMAT_UNCOMPRESSED_R8G8B8A8 as i32,
            mipmaps: 1,
            width: capture_width as i32,
            height: capture_height as i32,
        })
    };

    let screenshot_texture = rl
        .load_texture_from_image(&thread, &screenshot_image)
        .expect("failed to load screenshot into a texture");

    // Draw the full-resolution capture across the entire device framebuffer so
    // it fills the screen and stays sharp.
    let texture_src = Rectangle::new(0.0, 0.0, capture_width as f32, capture_height as f32);
    let texture_dst = Rectangle::new(0.0, 0.0, fb_width as f32, fb_height as f32);

    #[cfg(feature = "dev")]
    let mut spotlight_shader = rl
        .load_shader(&thread, None, Some("shaders/spotlight.fs"))
        .expect("Failed to load spotlight shader");

    #[cfg(not(feature = "dev"))]
    let mut spotlight_shader =
        rl.load_shader_from_memory(&thread, None, Some(include_str!("../shaders/spotlight.fs")));

    let mut rl_camera = Camera2D::default();
    rl_camera.zoom = 1.0;

    let mut delta_scale = 0f64;
    let mut scale_pivot = rl.get_mouse_position();
    let mut velocity = Vector2::default();
    let mut spotlight_radius_multiplier = 1.0;
    let mut spotlight_radius_multiplier_delta = 0.0;
    let mut spotlight_opacity = 0.0f32;

    #[cfg(feature = "dev")]
    let mut spotlight_tint_uniform_location;
    #[cfg(feature = "dev")]
    let mut cursor_position_uniform_location;
    #[cfg(feature = "dev")]
    let mut spotlight_radius_multiplier_uniform_location;
    #[cfg(feature = "dev")]
    let mut camera_zoom_uniform_location;
    #[cfg(feature = "dev")]
    let mut texture_size_uniform_location;
    #[cfg(not(feature = "dev"))]
    let spotlight_tint_uniform_location;
    #[cfg(not(feature = "dev"))]
    let cursor_position_uniform_location;
    #[cfg(not(feature = "dev"))]
    let spotlight_radius_multiplier_uniform_location;
    #[cfg(not(feature = "dev"))]
    let camera_zoom_uniform_location;
    #[cfg(not(feature = "dev"))]
    let texture_size_uniform_location;

    spotlight_tint_uniform_location = spotlight_shader.get_shader_location("spotlightTint");
    cursor_position_uniform_location = spotlight_shader.get_shader_location("cursorPosition");
    spotlight_radius_multiplier_uniform_location =
        spotlight_shader.get_shader_location("spotlightRadiusMultiplier");
    camera_zoom_uniform_location = spotlight_shader.get_shader_location("cameraZoom");
    texture_size_uniform_location = spotlight_shader.get_shader_location("textureSize");

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

        if rl.is_mouse_button_down(MouseButton::MOUSE_BUTTON_RIGHT) {
            break;
        }

        #[cfg(feature = "dev")]
        if rl.is_key_pressed(KeyboardKey::KEY_R) {
            spotlight_shader = rl
                .load_shader(&thread, None, Some("shaders/spotlight.fs"))
                .expect("Failed to load spotlight shader");
            spotlight_tint_uniform_location = spotlight_shader.get_shader_location("spotlightTint");
            cursor_position_uniform_location =
                spotlight_shader.get_shader_location("cursorPosition");
            spotlight_radius_multiplier_uniform_location =
                spotlight_shader.get_shader_location("spotlightRadiusMultiplier");
            camera_zoom_uniform_location = spotlight_shader.get_shader_location("cameraZoom");
            texture_size_uniform_location = spotlight_shader.get_shader_location("textureSize");
        }

        let enable_spotlight = rl.is_key_down(KeyboardKey::KEY_LEFT_CONTROL)
            || rl.is_key_down(KeyboardKey::KEY_RIGHT_CONTROL);

        let scrolled_amount = rl.get_mouse_wheel_move_v().y;
        let frame_time = rl.get_frame_time();

        let target_opacity = if enable_spotlight {
            SPOTLIGHT_TINT.a as f32 / 255.0
        } else {
            0.0
        };

        let fade_speed = 4.0f32;
        spotlight_opacity += (target_opacity - spotlight_opacity) * frame_time * fade_speed;

        if rl.is_key_pressed(KeyboardKey::KEY_LEFT_CONTROL)
            || rl.is_key_pressed(KeyboardKey::KEY_RIGHT_CONTROL)
        {
            spotlight_radius_multiplier = 5.0;
            spotlight_radius_multiplier_delta = -15.0;
        }

        if scrolled_amount != 0.0 {
            match (
                enable_spotlight,
                rl.is_key_down(KeyboardKey::KEY_LEFT_SHIFT)
                    || rl.is_key_down(KeyboardKey::KEY_RIGHT_SHIFT),
            ) {
                (_, false) => {
                    delta_scale += scrolled_amount as f64;
                }
                (true, true) => {
                    spotlight_radius_multiplier_delta -= scrolled_amount as f64;
                }
                _ => {}
            }
            scale_pivot = rl.get_mouse_position();
        }

        if delta_scale.abs() > 0.5 {
            let p0 = scale_pivot / rl_camera.zoom;
            rl_camera.zoom =
                (rl_camera.zoom as f64 + delta_scale * frame_time as f64).clamp(1.0, 10.0) as f32;
            let p1 = scale_pivot / rl_camera.zoom;
            rl_camera.target += p0 - p1;
            delta_scale -= delta_scale * frame_time as f64 * 4.0;
        }

        spotlight_radius_multiplier = (spotlight_radius_multiplier as f64
            + spotlight_radius_multiplier_delta * frame_time as f64)
            .clamp(0.3, 10.0) as f32;

        spotlight_radius_multiplier_delta -=
            spotlight_radius_multiplier_delta * frame_time as f64 * 4.0;

        const VELOCITY_THRESHOLD: f32 = 15.0;
        if rl.is_mouse_button_down(MouseButton::MOUSE_BUTTON_LEFT) {
            let mouse = rl.get_mouse_position();
            let prev_mouse = rl.get_mouse_position() - rl.get_mouse_delta();
            let delta = rl.get_screen_to_world2D(prev_mouse, rl_camera)
                - rl.get_screen_to_world2D(mouse, rl_camera);

            rl_camera.target += delta;
            velocity = delta * rl.get_fps().as_f32();
        } else if velocity.length_sqr() > VELOCITY_THRESHOLD * VELOCITY_THRESHOLD {
            rl_camera.target += velocity * frame_time;
            velocity -= velocity * frame_time * 6.0;
        }

        if rl.is_cursor_on_screen() {
            spotlight_mouse_position =
                rl.get_screen_to_world2D(rl.get_mouse_position(), rl_camera);
        }

        let mut d = rl.begin_drawing(&thread);
        let mut mode2d = d.begin_mode2D(rl_camera);
        // raylib leaves the viewport at the logical size; widen it to the real
        // device buffer so the scene fills the whole screen.
        unsafe { ffi::rlViewport(0, 0, fb_width, fb_height) };

        if enable_spotlight || spotlight_opacity > 0.001 {
            mode2d.clear_background(Color::get_color(0));

            let mouse_position = spotlight_mouse_position;

            spotlight_shader.set_shader_value(
                spotlight_tint_uniform_location,
                Vector4::new(
                    SPOTLIGHT_TINT.r as f32 / 255.0,
                    SPOTLIGHT_TINT.g as f32 / 255.0,
                    SPOTLIGHT_TINT.b as f32 / 255.0,
                    spotlight_opacity,
                ),
            );

            spotlight_shader.set_shader_value(
                cursor_position_uniform_location,
                Vector2::new(mouse_position.x, mouse_position.y),
            );

            spotlight_shader.set_shader_value(
                spotlight_radius_multiplier_uniform_location,
                spotlight_radius_multiplier,
            );
            spotlight_shader.set_shader_value(camera_zoom_uniform_location, rl_camera.zoom);
            // `fragTexCoord * textureSize` must land in the same space as
            // `cursorPosition` (raylib world space, which we drive in device
            // pixels), so use the device framebuffer size, not the raw capture.
            spotlight_shader.set_shader_value(
                texture_size_uniform_location,
                Vector2::new(fb_width as f32, fb_height as f32),
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
    }
}

fn print_help_and_exit(bin: &str) -> ! {
    eprintln!(
        "\
{bin}  – Wayland screen-zoom tool

USAGE:
    {bin} [--monitor <name>]

OPTIONS:
    --monitor <name>   Target monitor (Wayland output name); defaults to primary if flag is not provided.",
        bin = bin
    );
    process::exit(0);
}
