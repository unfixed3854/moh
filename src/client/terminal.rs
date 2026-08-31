use std::{
    fmt, io,
    panic::{self, PanicHookInfo},
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};

pub(super) trait EventSource {
    fn poll_event(&mut self, timeout: Duration) -> io::Result<Option<Event>>;
}

pub(super) struct CrosstermEvents;

impl EventSource for CrosstermEvents {
    fn poll_event(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        if crossterm::event::poll(timeout)? {
            Ok(map_event(crossterm::event::read()?))
        } else {
            Ok(None)
        }
    }
}

fn map_event(event: Event) -> Option<Event> {
    match event {
        Event::Key(key) if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => None,
        Event::Key(mut key) => {
            if key.code == KeyCode::Char('h') && key.modifiers.contains(KeyModifiers::CONTROL) {
                key.code = KeyCode::Backspace;
            }
            Some(Event::Key(key))
        }
        Event::Paste(_) | Event::Resize(_, _) => Some(event),
        Event::Mouse(mouse)
            if matches!(
                mouse.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            ) =>
        {
            Some(Event::Mouse(mouse))
        }
        Event::Mouse(_) | Event::FocusGained | Event::FocusLost => None,
    }
}

pub(super) trait ModeOps {
    type Terminal;

    fn init(&mut self) -> io::Result<Self::Terminal>;
    fn enable_paste(&mut self) -> io::Result<()>;
    fn enable_mouse(&mut self) -> io::Result<()>;
    fn disable_mouse(&mut self) -> io::Result<()>;
    fn disable_paste(&mut self) -> io::Result<()>;
    fn enable_keyboard_enhancement(&mut self) -> io::Result<bool>;
    fn disable_keyboard_enhancement(&mut self) -> io::Result<()>;
    fn restore(&mut self) -> io::Result<()>;
}

pub(super) struct ProductionModes;

impl ModeOps for ProductionModes {
    type Terminal = ratatui::DefaultTerminal;

    fn init(&mut self) -> io::Result<Self::Terminal> {
        // Ratatui replaces the process hook during initialization, so keep the
        // reset, initialization, and extra-mode wrapper indivisible.
        let _hook_guard = lock_panic_hook();
        prepare_ratatui_panic_hook_locked();
        let terminal = ratatui::try_init();
        install_extra_mode_panic_hook_locked();
        terminal
    }

    fn enable_paste(&mut self) -> io::Result<()> {
        crossterm::execute!(io::stdout(), EnableBracketedPaste)
    }

    fn enable_mouse(&mut self) -> io::Result<()> {
        crossterm::execute!(io::stdout(), EnableMouseCapture)
    }

    fn disable_mouse(&mut self) -> io::Result<()> {
        crossterm::execute!(io::stdout(), DisableMouseCapture)
    }

    fn disable_paste(&mut self) -> io::Result<()> {
        crossterm::execute!(io::stdout(), DisableBracketedPaste)
    }

    fn enable_keyboard_enhancement(&mut self) -> io::Result<bool> {
        if !matches!(
            crossterm::terminal::supports_keyboard_enhancement(),
            Ok(true)
        ) {
            return Ok(false);
        }
        crossterm::execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES,
            ),
        )?;
        PANIC_KEYBOARD_ENHANCEMENT_ENABLED.store(true, Ordering::Release);
        Ok(true)
    }

    fn disable_keyboard_enhancement(&mut self) -> io::Result<()> {
        crossterm::execute!(io::stdout(), PopKeyboardEnhancementFlags)?;
        PANIC_KEYBOARD_ENHANCEMENT_ENABLED.store(false, Ordering::Release);
        Ok(())
    }

    fn restore(&mut self) -> io::Result<()> {
        ratatui::try_restore()
    }
}

pub(super) struct TerminalSession<M: ModeOps> {
    modes: M,
    ratatui_started: bool,
    paste_enabled: bool,
    mouse_enabled: bool,
    keyboard_enhancement_enabled: bool,
    restored: bool,
}

impl<M: ModeOps> fmt::Debug for TerminalSession<M> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TerminalSession")
            .field("ratatui_started", &self.ratatui_started)
            .field("paste_enabled", &self.paste_enabled)
            .field("mouse_enabled", &self.mouse_enabled)
            .field(
                "keyboard_enhancement_enabled",
                &self.keyboard_enhancement_enabled,
            )
            .field("restored", &self.restored)
            .finish_non_exhaustive()
    }
}

impl TerminalSession<ProductionModes> {
    pub(super) fn start() -> io::Result<(ratatui::DefaultTerminal, Self)> {
        Self::start_with(ProductionModes)
    }
}

impl<M: ModeOps> TerminalSession<M> {
    fn start_with(modes: M) -> io::Result<(M::Terminal, Self)> {
        let mut session = Self {
            modes,
            ratatui_started: true,
            paste_enabled: false,
            mouse_enabled: false,
            keyboard_enhancement_enabled: false,
            restored: false,
        };

        let terminal = match session.modes.init() {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = session.restore();
                return Err(error);
            }
        };
        session.keyboard_enhancement_enabled = true;
        match session.modes.enable_keyboard_enhancement() {
            Ok(true) => {}
            Ok(false) => session.keyboard_enhancement_enabled = false,
            Err(error) => {
                let _ = session.restore();
                return Err(error);
            }
        }
        session.paste_enabled = true;
        if let Err(error) = session.modes.enable_paste() {
            let _ = session.restore();
            return Err(error);
        }
        session.mouse_enabled = true;
        if let Err(error) = session.modes.enable_mouse() {
            let _ = session.restore();
            return Err(error);
        }

        Ok((terminal, session))
    }

    pub(super) fn restore(&mut self) -> io::Result<()> {
        if self.restored {
            return Ok(());
        }

        let mut first_error = None;
        if self.mouse_enabled {
            let result = self.modes.disable_mouse();
            if Self::cleanup_succeeded(&mut first_error, result) {
                self.mouse_enabled = false;
            }
        }
        if self.paste_enabled {
            let result = self.modes.disable_paste();
            if Self::cleanup_succeeded(&mut first_error, result) {
                self.paste_enabled = false;
            }
        }
        if self.keyboard_enhancement_enabled {
            let result = self.modes.disable_keyboard_enhancement();
            if Self::cleanup_succeeded(&mut first_error, result) {
                self.keyboard_enhancement_enabled = false;
            }
        }
        if self.ratatui_started {
            let result = self.modes.restore();
            if Self::cleanup_succeeded(&mut first_error, result) {
                self.ratatui_started = false;
            }
        }
        self.restored = !self.mouse_enabled
            && !self.paste_enabled
            && !self.keyboard_enhancement_enabled
            && !self.ratatui_started;

        first_error.map_or(Ok(()), Err)
    }

    fn cleanup_succeeded(first_error: &mut Option<io::Error>, result: io::Result<()>) -> bool {
        match result {
            Ok(()) => true,
            Err(error) => {
                if first_error.is_none() {
                    *first_error = Some(error);
                }
                false
            }
        }
    }
}

impl<M: ModeOps> Drop for TerminalSession<M> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

type PanicHook = dyn Fn(&PanicHookInfo<'_>) + Send + Sync + 'static;

static PANIC_HOOK_LOCK: Mutex<()> = Mutex::new(());
static PREVIOUS_PANIC_HOOK: OnceLock<Arc<PanicHook>> = OnceLock::new();
static PANIC_KEYBOARD_ENHANCEMENT_ENABLED: AtomicBool = AtomicBool::new(false);

fn lock_panic_hook() -> MutexGuard<'static, ()> {
    PANIC_HOOK_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn prepare_ratatui_panic_hook_locked() {
    let previous = Arc::clone(PREVIOUS_PANIC_HOOK.get_or_init(|| Arc::from(panic::take_hook())));
    // Start every Ratatui initialization from the original delegate instead
    // of allowing prior Ratatui and extra-mode wrappers to accumulate.
    drop(panic::take_hook());
    panic::set_hook(Box::new(move |info| previous(info)));
}

fn install_extra_mode_panic_hook_locked() {
    let ratatui_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        restore_extra_modes_from_panic();
        ratatui_hook(info);
    }));
}

fn restore_extra_modes_from_panic() {
    attempt_extra_mode_panic_cleanup(
        || crossterm::execute!(io::stdout(), DisableMouseCapture),
        || crossterm::execute!(io::stdout(), DisableBracketedPaste),
        PANIC_KEYBOARD_ENHANCEMENT_ENABLED.swap(false, Ordering::AcqRel),
        || crossterm::execute!(io::stdout(), PopKeyboardEnhancementFlags),
    );
}

fn attempt_extra_mode_panic_cleanup(
    disable_mouse: impl FnOnce() -> io::Result<()>,
    disable_paste: impl FnOnce() -> io::Result<()>,
    keyboard_enhancement_enabled: bool,
    disable_keyboard: impl FnOnce() -> io::Result<()>,
) {
    let _ = disable_mouse();
    let _ = disable_paste();
    if keyboard_enhancement_enabled {
        let _ = disable_keyboard();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        collections::HashSet,
        env, io,
        panic::{AssertUnwindSafe, catch_unwind},
        process::Command,
        rc::Rc,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    };

    use super::*;

    const INIT: &str = "init";
    const ENABLE_KEYBOARD: &str = "enable_keyboard";
    const ENABLE_PASTE: &str = "enable_paste";
    const ENABLE_MOUSE: &str = "enable_mouse";
    const DISABLE_MOUSE: &str = "disable_mouse";
    const DISABLE_PASTE: &str = "disable_paste";
    const DISABLE_KEYBOARD: &str = "disable_keyboard";
    const RESTORE: &str = "restore";

    struct FakeModes {
        effects: Rc<RefCell<Vec<&'static str>>>,
        failures: HashSet<&'static str>,
        failures_once: RefCell<HashSet<&'static str>>,
        keyboard_enhancement_supported: bool,
    }

    impl FakeModes {
        fn new(effects: Rc<RefCell<Vec<&'static str>>>) -> Self {
            Self {
                effects,
                failures: HashSet::new(),
                failures_once: RefCell::new(HashSet::new()),
                keyboard_enhancement_supported: true,
            }
        }

        fn failing(effects: Rc<RefCell<Vec<&'static str>>>, failure: &'static str) -> Self {
            Self::failing_many(effects, [failure])
        }

        fn failing_many<const N: usize>(
            effects: Rc<RefCell<Vec<&'static str>>>,
            failures: [&'static str; N],
        ) -> Self {
            Self {
                effects,
                failures: failures.into_iter().collect(),
                failures_once: RefCell::new(HashSet::new()),
                keyboard_enhancement_supported: true,
            }
        }

        fn failing_once<const N: usize>(
            effects: Rc<RefCell<Vec<&'static str>>>,
            failures: [&'static str; N],
        ) -> Self {
            Self {
                effects,
                failures: HashSet::new(),
                failures_once: RefCell::new(failures.into_iter().collect()),
                keyboard_enhancement_supported: true,
            }
        }

        fn without_keyboard_enhancement(effects: Rc<RefCell<Vec<&'static str>>>) -> Self {
            Self {
                keyboard_enhancement_supported: false,
                ..Self::new(effects)
            }
        }

        fn run(&self, operation: &'static str) -> io::Result<()> {
            self.effects.borrow_mut().push(operation);
            if self.failures.contains(operation)
                || self.failures_once.borrow_mut().remove(operation)
            {
                Err(io::Error::other(format!("{operation} failed")))
            } else {
                Ok(())
            }
        }
    }

    impl ModeOps for FakeModes {
        type Terminal = ();

        fn init(&mut self) -> io::Result<Self::Terminal> {
            self.run(INIT)
        }

        fn enable_paste(&mut self) -> io::Result<()> {
            self.run(ENABLE_PASTE)
        }

        fn enable_mouse(&mut self) -> io::Result<()> {
            self.run(ENABLE_MOUSE)
        }

        fn disable_mouse(&mut self) -> io::Result<()> {
            self.run(DISABLE_MOUSE)
        }

        fn disable_paste(&mut self) -> io::Result<()> {
            self.run(DISABLE_PASTE)
        }

        fn enable_keyboard_enhancement(&mut self) -> io::Result<bool> {
            if !self.keyboard_enhancement_supported {
                return Ok(false);
            }
            self.run(ENABLE_KEYBOARD).map(|_| true)
        }

        fn disable_keyboard_enhancement(&mut self) -> io::Result<()> {
            self.run(DISABLE_KEYBOARD)
        }

        fn restore(&mut self) -> io::Result<()> {
            self.run(RESTORE)
        }
    }

    fn error_message<T>(result: io::Result<T>) -> String {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(error) => error.to_string(),
        }
    }

    fn key(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> Event {
        Event::Key(KeyEvent::new_with_kind(code, modifiers, kind))
    }

    fn mouse(kind: MouseEventKind) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: 4,
            row: 7,
            modifiers: KeyModifiers::ALT,
        })
    }

    #[test]
    fn maps_key_presses_and_repeats_but_ignores_releases() {
        let press = key(KeyCode::Char('x'), KeyModifiers::SHIFT, KeyEventKind::Press);
        let repeat = key(KeyCode::Left, KeyModifiers::ALT, KeyEventKind::Repeat);
        let release = key(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL,
            KeyEventKind::Release,
        );

        assert_eq!(map_event(press.clone()), Some(press));
        assert_eq!(map_event(repeat.clone()), Some(repeat));
        assert_eq!(map_event(release), None);
    }

    #[test]
    fn maps_shifted_enter_press() {
        let shifted_enter = key(KeyCode::Enter, KeyModifiers::SHIFT, KeyEventKind::Press);

        assert_eq!(map_event(shifted_enter.clone()), Some(shifted_enter));
    }

    #[test]
    fn normalizes_control_h_to_control_backspace() {
        let mapped = map_event(key(
            KeyCode::Char('h'),
            KeyModifiers::CONTROL,
            KeyEventKind::Press,
        ));

        assert_eq!(
            mapped,
            Some(key(
                KeyCode::Backspace,
                KeyModifiers::CONTROL,
                KeyEventKind::Press,
            ))
        );
    }

    #[test]
    fn maps_paste_resize_and_only_mouse_wheel_events() {
        let supported = [
            Event::Paste("界".into()),
            Event::Resize(120, 35),
            mouse(MouseEventKind::ScrollUp),
            mouse(MouseEventKind::ScrollDown),
        ];
        for event in supported {
            assert_eq!(map_event(event.clone()), Some(event));
        }

        let unsupported = [
            mouse(MouseEventKind::Down(MouseButton::Left)),
            mouse(MouseEventKind::Up(MouseButton::Left)),
            mouse(MouseEventKind::Drag(MouseButton::Left)),
            mouse(MouseEventKind::Moved),
            mouse(MouseEventKind::ScrollLeft),
            mouse(MouseEventKind::ScrollRight),
            Event::FocusGained,
            Event::FocusLost,
        ];
        for event in unsupported {
            assert_eq!(map_event(event), None);
        }
    }

    #[test]
    fn successful_session_restores_keyboard_mouse_paste_then_ratatui() {
        let effects = Rc::new(RefCell::new(Vec::new()));
        let modes = FakeModes::new(Rc::clone(&effects));
        let (_, mut session) = TerminalSession::start_with(modes).unwrap();

        session.restore().unwrap();

        assert_eq!(
            effects.borrow().as_slice(),
            [
                INIT,
                ENABLE_KEYBOARD,
                ENABLE_PASTE,
                ENABLE_MOUSE,
                DISABLE_MOUSE,
                DISABLE_PASTE,
                DISABLE_KEYBOARD,
                RESTORE,
            ]
        );
    }

    #[test]
    fn unsupported_keyboard_enhancement_preserves_existing_terminal_lifecycle() {
        let effects = Rc::new(RefCell::new(Vec::new()));
        let modes = FakeModes::without_keyboard_enhancement(Rc::clone(&effects));
        let (_, mut session) = TerminalSession::start_with(modes).unwrap();

        session.restore().unwrap();

        assert_eq!(
            effects.borrow().as_slice(),
            [
                INIT,
                ENABLE_PASTE,
                ENABLE_MOUSE,
                DISABLE_MOUSE,
                DISABLE_PASTE,
                RESTORE,
            ]
        );
    }

    #[test]
    fn every_setup_failure_conservatively_unwinds_attempted_acquisitions() {
        let cases: &[(&str, &[&str])] = &[
            (INIT, &[INIT, RESTORE]),
            (
                ENABLE_KEYBOARD,
                &[INIT, ENABLE_KEYBOARD, DISABLE_KEYBOARD, RESTORE],
            ),
            (
                ENABLE_PASTE,
                &[
                    INIT,
                    ENABLE_KEYBOARD,
                    ENABLE_PASTE,
                    DISABLE_PASTE,
                    DISABLE_KEYBOARD,
                    RESTORE,
                ],
            ),
            (
                ENABLE_MOUSE,
                &[
                    INIT,
                    ENABLE_KEYBOARD,
                    ENABLE_PASTE,
                    ENABLE_MOUSE,
                    DISABLE_MOUSE,
                    DISABLE_PASTE,
                    DISABLE_KEYBOARD,
                    RESTORE,
                ],
            ),
        ];

        for &(failure, expected) in cases {
            let effects = Rc::new(RefCell::new(Vec::new()));
            let modes = FakeModes::failing(Rc::clone(&effects), failure);

            assert_eq!(
                error_message(TerminalSession::start_with(modes)),
                format!("{failure} failed")
            );
            assert_eq!(
                effects.borrow().as_slice(),
                expected,
                "failure at {failure}"
            );
        }
    }

    #[test]
    fn setup_error_wins_and_drop_retries_failed_unwind_actions() {
        let effects = Rc::new(RefCell::new(Vec::new()));
        let mut modes =
            FakeModes::failing_once(Rc::clone(&effects), [ENABLE_MOUSE, DISABLE_PASTE, RESTORE]);
        modes.failures.insert(ENABLE_MOUSE);

        assert_eq!(
            error_message(TerminalSession::start_with(modes)),
            "enable_mouse failed"
        );
        assert_eq!(
            effects.borrow().as_slice(),
            [
                INIT,
                ENABLE_KEYBOARD,
                ENABLE_PASTE,
                ENABLE_MOUSE,
                DISABLE_MOUSE,
                DISABLE_PASTE,
                DISABLE_KEYBOARD,
                RESTORE,
                DISABLE_PASTE,
                RESTORE,
            ]
        );
    }

    #[test]
    fn each_cleanup_failure_is_returned_after_every_inverse_is_attempted() {
        for failure in [DISABLE_MOUSE, DISABLE_PASTE, DISABLE_KEYBOARD, RESTORE] {
            let effects = Rc::new(RefCell::new(Vec::new()));
            let modes = FakeModes::failing(Rc::clone(&effects), failure);
            let (_, mut session) = TerminalSession::start_with(modes).unwrap();

            assert_eq!(
                error_message(session.restore()),
                format!("{failure} failed")
            );
            assert_eq!(
                &effects.borrow()[4..],
                [DISABLE_MOUSE, DISABLE_PASTE, DISABLE_KEYBOARD, RESTORE],
                "failure at {failure}"
            );
        }
    }

    #[test]
    fn restore_attempts_every_inverse_and_returns_the_first_error() {
        let effects = Rc::new(RefCell::new(Vec::new()));
        let modes = FakeModes::failing_many(
            Rc::clone(&effects),
            [DISABLE_MOUSE, DISABLE_PASTE, DISABLE_KEYBOARD, RESTORE],
        );
        let (_, mut session) = TerminalSession::start_with(modes).unwrap();

        assert_eq!(error_message(session.restore()), "disable_mouse failed");
        assert!(effects.borrow().ends_with(&[
            DISABLE_MOUSE,
            DISABLE_PASTE,
            DISABLE_KEYBOARD,
            RESTORE
        ]));
    }

    #[test]
    fn restore_retries_only_failed_cleanup_then_becomes_idempotent() {
        let effects = Rc::new(RefCell::new(Vec::new()));
        let modes = FakeModes::failing_once(
            Rc::clone(&effects),
            [DISABLE_MOUSE, DISABLE_KEYBOARD, RESTORE],
        );
        let (_, mut session) = TerminalSession::start_with(modes).unwrap();

        assert_eq!(error_message(session.restore()), "disable_mouse failed");
        session.restore().unwrap();
        session.restore().unwrap();
        drop(session);

        assert_eq!(
            effects.borrow().as_slice(),
            [
                INIT,
                ENABLE_KEYBOARD,
                ENABLE_PASTE,
                ENABLE_MOUSE,
                DISABLE_MOUSE,
                DISABLE_PASTE,
                DISABLE_KEYBOARD,
                RESTORE,
                DISABLE_MOUSE,
                DISABLE_KEYBOARD,
                RESTORE,
            ]
        );
    }

    #[test]
    fn drop_attempts_best_effort_cleanup_without_panicking() {
        let effects = Rc::new(RefCell::new(Vec::new()));
        let modes = FakeModes::failing_many(
            Rc::clone(&effects),
            [DISABLE_MOUSE, DISABLE_PASTE, DISABLE_KEYBOARD, RESTORE],
        );

        let dropped = catch_unwind(AssertUnwindSafe(|| {
            let _session = TerminalSession::start_with(modes).unwrap();
        }));

        assert!(dropped.is_ok());
        assert_eq!(
            &effects.borrow()[4..],
            [DISABLE_MOUSE, DISABLE_PASTE, DISABLE_KEYBOARD, RESTORE]
        );
    }

    #[test]
    fn panic_cleanup_attempts_paste_after_mouse_failure() {
        let effects = RefCell::new(Vec::new());

        attempt_extra_mode_panic_cleanup(
            || {
                effects.borrow_mut().push(DISABLE_MOUSE);
                Err(io::Error::other("mouse failed"))
            },
            || {
                effects.borrow_mut().push(DISABLE_PASTE);
                Ok(())
            },
            true,
            || {
                effects.borrow_mut().push(DISABLE_KEYBOARD);
                Ok(())
            },
        );

        assert_eq!(
            effects.borrow().as_slice(),
            [DISABLE_MOUSE, DISABLE_PASTE, DISABLE_KEYBOARD]
        );
    }

    #[test]
    fn panic_cleanup_leaves_unowned_keyboard_enhancement_untouched() {
        let effects = RefCell::new(Vec::new());

        attempt_extra_mode_panic_cleanup(
            || {
                effects.borrow_mut().push(DISABLE_MOUSE);
                Ok(())
            },
            || {
                effects.borrow_mut().push(DISABLE_PASTE);
                Ok(())
            },
            false,
            || {
                effects.borrow_mut().push(DISABLE_KEYBOARD);
                Ok(())
            },
        );

        assert_eq!(effects.borrow().as_slice(), [DISABLE_MOUSE, DISABLE_PASTE]);
    }

    #[test]
    fn production_panic_hook_remains_bounded_across_ratatui_reinstallations() {
        const PROBE: &str = "MOH_TERMINAL_PANIC_HOOK_PROBE";
        if env::var_os(PROBE).is_some() {
            static PREVIOUS_CALLS: AtomicUsize = AtomicUsize::new(0);
            static RATATUI_CALLS: AtomicUsize = AtomicUsize::new(0);

            std::panic::set_hook(Box::new(|_| {
                PREVIOUS_CALLS.fetch_add(1, Ordering::SeqCst);
            }));

            for _ in 0..3 {
                let _hook_guard = lock_panic_hook();
                prepare_ratatui_panic_hook_locked();
                let previous = std::panic::take_hook();
                std::panic::set_hook(Box::new(move |info| {
                    RATATUI_CALLS.fetch_add(1, Ordering::SeqCst);
                    previous(info);
                }));
                install_extra_mode_panic_hook_locked();
            }

            assert!(catch_unwind(|| panic!("panic hook probe")).is_err());
            assert_eq!(PREVIOUS_CALLS.load(Ordering::SeqCst), 1);
            assert_eq!(RATATUI_CALLS.load(Ordering::SeqCst), 1);
            return;
        }

        let output = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "client::terminal::tests::production_panic_hook_remains_bounded_across_ratatui_reinstallations",
                "--nocapture",
            ])
            .env(PROBE, "1")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "panic-hook probe failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
