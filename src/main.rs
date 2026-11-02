//! clickflash: a black window that shows a white frame for each click or key press, for
//! measuring click-to-photon latency. Every input and flash can be logged with timestamps.

mod gpu;

use std::fs::File;
use std::io::{BufWriter, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

pub struct Options {
	pub duration: Option<Duration>,
	pub fullscreen: bool,
	pub size: (u32, u32),
	pub vsync: bool,
	pub flash_frames: u32,
	pub log: Option<String>,
}

const USAGE: &str = "\
usage: clickflash [options]

  --flash-frames N        frames each flash lasts (default 1)
  --log PATH              write every input and flash as a JSON line to PATH (- for stdout)
  --duration SECONDS      exit after SECONDS
  --fullscreen            borderless fullscreen
  --size WxH              window size in pixels (default 1920x1080)
  --no-vsync              present as fast as possible (mailbox or immediate)

a click (any mouse button) or a key press flashes the window white. q or escape quits.
times are CLOCK_MONOTONIC_RAW microseconds; a summary is printed on exit.";

fn parse_args() -> Result<Options, String> {
	let mut o = Options {
		duration: None,
		fullscreen: false,
		size: (1920, 1080),
		vsync: true,
		flash_frames: 1,
		log: None,
	};
	let mut args = std::env::args().skip(1);
	while let Some(arg) = args.next() {
		match arg.as_str() {
			"--flash-frames" => {
				o.flash_frames = args
					.next()
					.and_then(|v| v.parse().ok())
					.filter(|n| *n >= 1)
					.ok_or("--flash-frames needs a number of frames, at least 1")?;
			},
			"--log" => o.log = Some(args.next().ok_or("--log needs a path, or - for stdout")?),
			"--duration" => {
				o.duration = Some(
					args.next()
						.and_then(|v| v.parse::<f64>().ok())
						.filter(|s| *s > 0.0)
						.map(Duration::from_secs_f64)
						.ok_or("--duration needs a number of seconds")?,
				);
			},
			"--fullscreen" => o.fullscreen = true,
			"--size" => {
				let v = args.next().unwrap_or_default();
				o.size = v
					.split_once('x')
					.and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
					.ok_or_else(|| format!("--size needs WxH, got {v:?}"))?;
			},
			"--no-vsync" => o.vsync = false,
			"-h" | "--help" => {
				println!("{USAGE}");
				std::process::exit(0);
			},
			_ => return Err(format!("unknown option {arg:?}")),
		}
	}
	Ok(o)
}

/// Set by SIGTERM and SIGINT: quit like `q`, so the log is complete and the summary printed
/// (a streaming server stops its apps with SIGTERM).
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_stop(_: libc::c_int) {
	STOP.store(true, Ordering::Relaxed);
}

/// Microseconds on CLOCK_MONOTONIC_RAW, the clock moonlight-common-c uses, so other processes
/// on the machine can line their timestamps up with these.
pub fn now_us() -> u64 {
	let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
	// SAFETY: clock_gettime only writes the timespec it's given.
	unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &mut ts) };
	ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
}

struct App {
	options: Options,
	gpu: Option<gpu::Gpu>,
	start: Instant,
	log: Option<Box<dyn Write>>,
	/// Inputs received and flashes presented, numbered from 1.
	inputs: u64,
	flashes: u64,
	/// The input a flash is pending for: its number and when it arrived.
	pending: Option<(u64, u64)>,
	/// White frames still to present for the current flash.
	white_left: u32,
	/// Inputs that came while a flash was still pending or showing, and didn't get their own.
	merged: u64,
	/// Input-to-submit times, ms.
	latencies: Vec<f64>,
	error: Option<String>,
}

impl App {
	fn fail(&mut self, event_loop: &ActiveEventLoop, error: String) {
		self.error = Some(error);
		event_loop.exit();
	}

	fn log_line(&mut self, line: String) {
		// Flushed per line, so a reader following the log sees each event as it happens.
		if let Some(log) = &mut self.log
			&& let Err(e) = writeln!(log, "{line}").and_then(|()| log.flush())
		{
			eprintln!("clickflash: writing the log failed: {e}");
			self.log = None;
		}
	}

	fn input(&mut self, kind: &str) {
		let t = now_us();
		self.inputs += 1;
		let n = self.inputs;
		self.log_line(format!(r#"{{"event":"input","input":{n},"kind":"{kind}","t_us":{t}}}"#));
		if self.pending.is_some() || self.white_left > 0 {
			self.merged += 1;
			return;
		}
		self.pending = Some((n, t));
		self.white_left = self.options.flash_frames;
	}

	fn frame(&mut self, event_loop: &ActiveEventLoop) {
		if STOP.load(Ordering::Relaxed) || self.options.duration.is_some_and(|d| self.start.elapsed() >= d) {
			event_loop.exit();
			return;
		}
		let Some(gpu) = &mut self.gpu else { return };
		let white = self.white_left > 0;
		match gpu.draw(white) {
			Ok(None) => {},
			Ok(Some(t)) => {
				if white {
					self.white_left -= 1;
				}
				// The first white frame of a flash is the one a sensor or a client sees.
				if let Some((input, input_us)) = self.pending.take() {
					self.flashes += 1;
					let flash = self.flashes;
					self.latencies.push((t - input_us) as f64 / 1000.0);
					self.log_line(format!(
						r#"{{"event":"flash","flash":{flash},"input":{input},"t_us":{t},"input_to_submit_us":{}}}"#,
						t - input_us
					));
				}
			},
			Err(e) => self.fail(event_loop, e),
		}
	}

	fn report(&mut self) {
		if let Some(log) = &mut self.log {
			let _ = log.flush();
		}
		let t = &mut self.latencies;
		if t.is_empty() {
			eprintln!("clickflash: {} inputs, no flashes", self.inputs);
			return;
		}
		t.sort_by(f64::total_cmp);
		let at = |q: f64| t[((t.len() - 1) as f64 * q) as usize];
		eprintln!(
			"clickflash: {} inputs, {} flashes ({} merged into one already showing); input to submit p50 {:.2} ms, p99 {:.2} ms, max {:.2} ms",
			self.inputs,
			self.flashes,
			self.merged,
			at(0.5),
			at(0.99),
			t[t.len() - 1],
		);
	}
}

impl ApplicationHandler for App {
	fn resumed(&mut self, event_loop: &ActiveEventLoop) {
		if self.gpu.is_some() {
			return;
		}
		let (w, h) = self.options.size;
		let mut attributes = Window::default_attributes()
			.with_title("clickflash")
			.with_inner_size(PhysicalSize::new(w, h));
		if self.options.fullscreen {
			attributes = attributes.with_fullscreen(Some(Fullscreen::Borderless(None)));
		}
		let window = match event_loop.create_window(attributes) {
			Ok(window) => Arc::new(window),
			Err(e) => return self.fail(event_loop, format!("couldn't create a window: {e}")),
		};
		match gpu::Gpu::new(window, &self.options) {
			Ok(gpu) => {
				eprintln!("clickflash: on {}", gpu.device_name());
				self.gpu = Some(gpu);
				self.start = Instant::now();
			},
			Err(e) => self.fail(event_loop, e),
		}
	}

	fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
		match event {
			WindowEvent::CloseRequested => event_loop.exit(),
			WindowEvent::Resized(_) => {
				if let Some(gpu) = &mut self.gpu {
					gpu.resized();
				}
			},
			WindowEvent::MouseInput {
				state: ElementState::Pressed,
				..
			} => self.input("click"),
			WindowEvent::KeyboardInput {
				event: KeyEvent {
					logical_key,
					state: ElementState::Pressed,
					repeat: false,
					..
				},
				..
			} => match logical_key.as_ref() {
				Key::Named(NamedKey::Escape) | Key::Character("q") => event_loop.exit(),
				_ => self.input("key"),
			},
			WindowEvent::RedrawRequested => self.frame(event_loop),
			_ => {},
		}
	}

	fn about_to_wait(&mut self, _: &ActiveEventLoop) {
		if let Some(gpu) = &self.gpu {
			gpu.window().request_redraw();
		}
	}
}

fn main() -> ExitCode {
	let options = match parse_args() {
		Ok(o) => o,
		Err(e) => {
			eprintln!("clickflash: {e}\n\n{USAGE}");
			return ExitCode::from(2);
		},
	};
	// SAFETY: the handler only stores to an atomic.
	unsafe {
		libc::signal(libc::SIGTERM, on_stop as *const () as libc::sighandler_t);
		libc::signal(libc::SIGINT, on_stop as *const () as libc::sighandler_t);
	}
	let log: Option<Box<dyn Write>> = match options.log.as_deref() {
		None => None,
		Some("-") => Some(Box::new(std::io::stdout())),
		Some(path) => match File::create(path) {
			Ok(f) => Some(Box::new(BufWriter::new(f))),
			Err(e) => {
				eprintln!("clickflash: {path}: {e}");
				return ExitCode::FAILURE;
			},
		},
	};
	let event_loop = match EventLoop::new() {
		Ok(l) => l,
		Err(e) => {
			eprintln!("clickflash: no display: {e}");
			return ExitCode::FAILURE;
		},
	};
	let mut app = App {
		options,
		gpu: None,
		start: Instant::now(),
		log,
		inputs: 0,
		flashes: 0,
		pending: None,
		white_left: 0,
		merged: 0,
		latencies: Vec::new(),
		error: None,
	};
	if let Err(e) = event_loop.run_app(&mut app) {
		eprintln!("clickflash: {e}");
		return ExitCode::FAILURE;
	}
	app.report();
	match app.error {
		Some(e) => {
			eprintln!("clickflash: {e}");
			ExitCode::FAILURE
		},
		None => ExitCode::SUCCESS,
	}
}
