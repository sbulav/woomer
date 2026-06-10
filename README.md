# Woomer - Boomer but for wayland

Zoomer application for wayland (linux) inspired by [tsoding's boomer](https://github.com/tsoding/boomer) written in rust.

> **This is a fork** of [coffeeispower/woomer](https://github.com/coffeeispower/woomer)
> with fixes for HiDPI / fractionally-scaled outputs and multi-monitor setups.
> See [Fork fixes](#fork-fixes) below.

## Fork fixes

This fork carries the following fixes on top of upstream:

- Correct fullscreen capture on fractionally-scaled outputs (e.g. 1.25/1.5 scaling).
- Fixed spotlight and pan alignment on scaled outputs (the flashlight and the
  dragged image now track the cursor correctly).
- Fixed fullscreen coverage so the zoom surface spans the whole scaled output.
- Fixed the cursor-spotlight bug on Hyprland and smoothed the spotlight transition.
- Fixed image stitching and per-monitor image selection in multi-monitor setups.
- Fixed the first-frame glitch on startup.
- Dropped `libwayshot` in favour of `grim-rs` for screen capture.

## Controls

| Control                                                   | Description                                  |
|-----------------------------------------------------------|----------------------------------------------|
| Right Click, <kbd>ESC</kbd>, <kbd>A</kbd> or <kbd>Q</kbd> | Quit the application.                        |
| <kbd>R</kbd>                                              | Reload the shaders (only for Developer mode) |
| Hold <kbd>CTRL</kbd>                                      | Enable flashlight effect.                    |
| Drag with left mouse button                               | Move the image around.                       |
| Scroll wheel                                              | Zoom in/out.                                 |
| <kbd>Ctrl</kbd> + <kbd>SHIFT</kbd> + Scroll wheel         | Change the radius of the flashlight.         |

## HiDPI Displays
### Hyprland
If you set your scaling different to 1.0 in your .config/hypr/hyprland.conf, you also need the following variables to be set:
```sh
$scale = 2
monitor = , highres, auto, $scale

xwayland {
  force_zero_scaling = true
}
env = GDK_SCALE,$scale
```

Please also refer to: [Hyprland Wiki](https://wiki.hyprland.org/Configuring/XWayland/)

## Building

Dependencies:

- cmake
- clang (for compiling raylib)
- glfw (for fullscreening)
- libclang (for bindgen)
- pkg-config
- rust
- wayland-client

Like with any other rust program you can run:

```sh
cargo b
```

However if you want hot reloading of the spotlight shader you can add the `dev` feature:

```sh
cargo b -F dev
```

## Installing using the Nix flake

You can also install woomer using the nix flake:

```nix
# flake.nix
{
  inputs = {
    woomer.url = "github:sbulav/woomer";
    # .....
  }
  # ....
}
```

After that, you can just install it

```nix
{inputs, system, ...}:
{
  home.packages = [
    inputs.woomer.packages.${system}.default
    # ....
  ];
  # ....
}
```
