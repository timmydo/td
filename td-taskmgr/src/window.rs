//! Wayland adapter; collection and input scheduling do not depend on frames.
use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use td_taskmgr::budget::{Budget, LIMIT};
use td_taskmgr::history::Interval;
use td_taskmgr::ui::{Outcome, Phase, State};
use td_taskmgr::worker::Worker;
use td_ui::client::{App, Client, Handled, KeyboardEvent, Tag};
use td_ui::font::Font;
use td_ui::pointer::{self, Wheel};
use td_ui::raster::{Raster, Scale, Surface};
use td_ui::wire::Message;
type Result<T> = std::result::Result<T, String>;
fn error(e: impl std::fmt::Display) -> String {
    e.to_string()
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Object {}
impl Tag for Object {
    fn retired(self) -> bool {
        match self {}
    }
}
struct Window {
    client: Client<Object>,
    font: Font,
    state: State,
    worker: Option<Worker>,
    origin: Instant,
    quit_at: Option<u64>,
    size: (usize, usize),
    clock: u64,
    pointer: (i64, i64),
    wheel: Wheel,
    last_collection: u64,
    control: Option<td_ui::control_worker::Worker<td_ui::driven::Payload>>,
    remote_pointer: (i64, i64),
    presentations: u64,
}
impl Window {
    fn new(
        stream: UnixStream,
        temporary: PathBuf,
        control: Option<td_ui::control_worker::Worker<td_ui::driven::Payload>>,
    ) -> Result<Self> {
        let budget = Budget::new(LIMIT).map_err(error)?;
        let surface = Surface::new(1280, 960, Scale::default()).map_err(error)?;
        let mut state = State::new(&budget, surface)?;
        let worker = match Worker::start(&budget, 1, Interval::Second) {
            Ok(worker) => Some(worker),
            Err(why) => {
                state.note(&format!("Collection unavailable: {why}"));
                None
            }
        };
        Ok(Self {
            client: Client::new(stream, temporary)?,
            font: td_ui::font::pinned()?,
            state,
            worker,
            origin: Instant::now(),
            quit_at: None,
            size: (1280, 960),
            clock: 0,
            pointer: (0, 0),
            wheel: Wheel::default(),
            last_collection: 0,
            control,
            remote_pointer: (0, 0),
            presentations: 0,
        })
    }
    fn apply(&mut self, outcome: Outcome) -> Result<()> {
        match outcome {
            Outcome::Quit => self.client.close(),
            Outcome::Interval(interval) => {
                if let Some(worker) = &self.worker {
                    if let Err(why) = worker.interval(interval) {
                        self.state.note(&format!("Collection unavailable: {why}"));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn key(&mut self, chord: &str, repeated: bool) -> Result<Outcome> {
        let outcome = self.state.key(chord, repeated);
        self.apply(outcome)?;
        Ok(outcome)
    }
    fn pointer(&mut self, event: pointer::Event) -> Result<()> {
        match event {
            pointer::Event::Enter { x, y, .. } | pointer::Event::Motion(x, y) => {
                self.pointer = (i64::from(x).div_euclid(256), i64::from(y).div_euclid(256));
                let outcome = self
                    .state
                    .pointer(Phase::Move, self.pointer.0, self.pointer.1);
                self.apply(outcome)?;
            }
            pointer::Event::Leave(_) => self.state.cancel_gesture(),
            pointer::Event::Button {
                button: 272,
                pressed,
                ..
            } => {
                let phase = if pressed {
                    Phase::Press
                } else {
                    Phase::Release
                };
                let outcome = self.state.pointer(phase, self.pointer.0, self.pointer.1);
                self.apply(outcome)?;
            }
            pointer::Event::Button { .. } => {}
            pointer::Event::Frame => {
                let (rows, cols) = self.wheel.frame();
                if rows != 0 || cols != 0 {
                    let outcome =
                        self.state
                            .scroll(self.pointer.0, self.pointer.1, rows as i64, cols as i64);
                    self.apply(outcome)?;
                }
            }
            _ => self.wheel.update(event)?,
        }
        Ok(())
    }
}
impl App for Window {
    type Tag = Object;
    fn client(&mut self) -> &mut Client<Object> {
        &mut self.client
    }
    fn needs_descriptor(&self, _: &Message) -> Result<bool> {
        Ok(false)
    }
    fn descriptor_wait(&mut self) {
        self.state.cancel_gesture();
    }
    fn tick(&mut self, now: u64) -> Result<()> {
        self.clock = now;
        Ok(())
    }
    fn event(&mut self, message: Message) -> Result<()> {
        match self.client.handle(&message, self.clock)? {
            Handled::Bound => {
                self.client.set_title("Task Manager")?;
                self.client.set_app_id("td-taskmgr")?;
                self.client.commit()?;
            }
            Handled::Configure { size, serial } => {
                if let Some((width, height)) = size {
                    if width > 0 {
                        self.size.0 = width as usize;
                    }
                    if height > 0 {
                        self.size.1 = height as usize;
                    }
                }
                match Surface::new(self.size.0, self.size.1, Scale::default()) {
                    Ok(surface) => self.state.resize(surface)?,
                    Err(why) => {
                        let current = self.state.surface();
                        self.size = (current.width, current.height);
                        self.state.cancel_gesture();
                        self.state.note(&format!(
                            "Requested window size exceeds the surface limit: {why}"
                        ));
                    }
                }
                self.client.acknowledge(serial)?;
            }
            Handled::CloseRequested => self.client.close(),
            Handled::Keyboard(event) => match event {
                KeyboardEvent::Key { key, stroke, .. } => {
                    let outcome = self.key(&stroke.chord, false)?;
                    if stroke.repeat
                        && outcome != Outcome::Ignored
                        && !matches!(outcome, Outcome::Quit | Outcome::Interval(_))
                    {
                        self.client.arm(key, self.clock);
                    }
                }
                KeyboardEvent::Focus(false) => self.state.cancel(),
                KeyboardEvent::Focus(true) => self.state.restore_focus(),
                KeyboardEvent::Keymap(Err(why)) | KeyboardEvent::Refused(why) => {
                    let _ = writeln!(io::stderr().lock(), "td-taskmgr: {why}");
                }
                _ => {}
            },
            Handled::Pointer(event) => self.pointer(event)?,
            Handled::SeatRemoved => self.state.cancel(),
            Handled::Capabilities {
                keyboard: false, ..
            } => self.state.cancel(),
            Handled::Capabilities { pointer: false, .. } => self.state.cancel_gesture(),
            Handled::GlobalRemoved { required: true, .. } => {
                return Err("required Wayland global was removed".into())
            }
            Handled::Unhandled => {
                return Err(format!(
                    "unexpected Wayland event {}:{}",
                    message.object, message.opcode
                ))
            }
            _ => {}
        }
        Ok(())
    }
    fn end_turn(&mut self, now: u64, idle: bool) -> Result<()> {
        self.clock = now;
        if idle {
            if let Some(stroke) = self.client.repeat(now)? {
                if self.key(&stroke.chord, true)? == Outcome::Ignored {
                    self.client.cancel_repeat();
                }
            }
        }
        if now.saturating_sub(self.last_collection) >= 50 {
            self.last_collection = now;
            if let Some(worker) = &mut self.worker {
                match worker
                    .take()
                    .and_then(|update| Ok((update, worker.elapsed_ns()?)))
                {
                    Ok((update, now_ns)) => self.state.update(update, now_ns),
                    Err(why) => self.state.note(&format!("Collection unavailable: {why}")),
                }
            } else {
                self.state.update(
                    td_taskmgr::worker::Update {
                        batch: None,
                        skipped: 0,
                        failure: None,
                    },
                    self.origin.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
                );
            }
        }
        if self.quit_at.is_some_and(|at| now >= at) {
            self.client.close();
        }
        for _ in 0..if self.quit_at.is_some() { 0 } else { 4 } {
            let job = match self.control.as_ref() {
                Some(worker) => match worker.try_request() {
                    Ok(job) => job,
                    Err(why) => {
                        let _ = writeln!(io::stderr().lock(), "td-taskmgr: control stopped: {why}");
                        self.control = None;
                        None
                    }
                },
                None => None,
            };
            let Some(job) = job else { break };
            let mut remote = td_taskmgr::control::Remote {
                state: &mut self.state,
                pointer: &mut self.remote_pointer,
                effect: None,
                presentations: self.presentations,
            };
            let answer =
                job.respond_with(|payload| td_ui::driven::request(&mut remote, payload.bytes()));
            let effect = remote.effect;
            if let Err(why) = answer {
                let _ = writeln!(io::stderr().lock(), "td-taskmgr: control reply: {why}");
            }
            if let Some(effect) = effect {
                if effect == Outcome::Quit {
                    self.quit_at = Some(now.saturating_add(200));
                    break;
                }
                self.apply(effect)?;
            }
        }
        let wait = self.client.wait_ms(now).min(50);
        self.client
            .connection()
            .set_wait(Duration::from_millis(wait));
        Ok(())
    }
    fn draw(&mut self) -> Result<()> {
        if !self.state.dirty() || !self.client.can_present() {
            return Ok(());
        }
        let surface = self.state.surface();
        let state = &self.state;
        let font = &self.font;
        if self
            .client
            .present(surface.width, surface.height, &mut |pixels| {
                let mut raster =
                    Raster::new(pixels, font, surface, surface.width * 4).map_err(error)?;
                state.emit(surface.bounds(), &mut |draw| raster.draw(draw));
                Ok(())
            })?
        {
            self.state.painted();
            self.presentations = self.presentations.saturating_add(1);
        }
        Ok(())
    }
}
pub fn open(control_path: Option<PathBuf>) -> io::Result<()> {
    let work = || -> Result<()> {
        let control = control_path
            .as_deref()
            .map(td_ui::control_socket::Socket::bind)
            .transpose()
            .map_err(error)?
            .map(td_ui::control_worker::Worker::start)
            .transpose()
            .map_err(error)?;
        let endpoint = td_ui::wayland::endpoint(
            std::env::var_os("WAYLAND_SOCKET"),
            std::env::var_os("WAYLAND_DISPLAY"),
            std::env::var_os("XDG_RUNTIME_DIR"),
        )?;
        let stream = td_ui::wayland::connect(endpoint)?;
        let mut window = Window::new(stream, std::env::temp_dir(), control)?;
        td_ui::client::run(&mut window)
    };
    work().map_err(io::Error::other)
}
pub fn preview(width: usize, height: usize) -> io::Result<()> {
    let budget = Budget::new(LIMIT).map_err(io::Error::other)?;
    let surface = Surface::new(width, height, Scale::default()).map_err(io::Error::other)?;
    let mut state = State::new(&budget, surface).map_err(io::Error::other)?;
    let mut collector = td_taskmgr::collector::Collector::new(&budget, 1)?;
    for _ in 0..2 {
        let batch = collector.sample(&std::sync::atomic::AtomicBool::new(false))?;
        let now = batch.ended_ns;
        state.update(
            td_taskmgr::worker::Update {
                batch: Some(batch),
                skipped: 0,
                failure: None,
            },
            now,
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let font = td_ui::font::pinned().map_err(io::Error::other)?;
    let mut pixels = vec![0u8; surface.width * surface.height * 4];
    let mut raster =
        Raster::new(&mut pixels, &font, surface, surface.width * 4).map_err(io::Error::other)?;
    state.emit(surface.bounds(), &mut |draw| raster.draw(draw));
    let rgb = td_ui::raster::rgb(&pixels, surface, surface.width * 4).map_err(io::Error::other)?;
    io::stdout()
        .lock()
        .write_all(&td_ui::raster::ppm(surface, &rgb))
}
