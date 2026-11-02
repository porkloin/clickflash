# clickflash: details

What README.md leaves out: what exactly happens on a click, the log format, what the times mean, and the layout.
README.md is the short version for people.

## Behavior

- The window is cleared to black every frame. A mouse button press (any button) or a key press (not a repeat; Q
  and Escape quit) makes the next `--flash-frames` frames (1) white: the whole window, so a sensor or a client can
  watch any part of it.
- An input that arrives while a flash is pending or still showing gets no flash of its own; it's counted as merged.
- It redraws continuously, so a capturing compositor always has new frames.
- SIGTERM and SIGINT quit like Q (a streaming server stops its apps with SIGTERM), so the log is complete and the
  summary is printed.
- `--no-vsync` presents with mailbox or immediate instead of FIFO.

## Log

`--log PATH` (`-` for stdout) writes one JSON object per line, flushed as it happens:

```text
{"event":"input","input":1,"kind":"click","t_us":123456789}
{"event":"flash","flash":1,"input":1,"t_us":123456870,"input_to_submit_us":81}
```

- `t_us` is microseconds on `CLOCK_MONOTONIC_RAW`, the clock moonlight-common-c uses, so other processes on the
  machine can line their times up with these.
- `input` is when the event loop received the input; winit gives no earlier timestamp.
- A flash's `t_us` is when its first white frame was submitted to the GPU. It's the last point the app controls:
  with vsync, `queue_present` can block for up to a frame afterwards, so present-return time would overstate the
  app's part.

On exit it prints the counts and input-to-submit percentiles to stderr.

## Layout

- `src/main.rs`: options, the event loop, inputs, flashes, the log.
- `src/gpu.rs`: Vulkan; each frame is a dynamic-rendering pass that only clears the swapchain image (no shaders).
- `.github/workflows/`: CI (fmt, clippy, build), and release-please with release tarballs and `SHA256SUMS`.

Check with `cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings`.
