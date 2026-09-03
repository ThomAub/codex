use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use tempfile::TempDir;

// Full startup continues after the composer first appears and can be slower under Rosetta in CI.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 30);
const FOCUS_INPUT_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 5);
const FOCUS_PROBE_INPUT: &str = "focus-palette-24527";
const LIGHT_PALETTE_RESPONSE: &[u8] =
    b"\x1b]10;rgb:0000/0000/0000\x1b\\\x1b]11;rgb:ffff/ffff/ffff\x1b\\";
const DARK_PALETTE_RESPONSE: &[u8] =
    b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\\x1b]11;rgb:0000/0000/0000\x1b\\";
const LIGHT_COMPOSER_BACKGROUND: vt100::Color = vt100::Color::Rgb(244, 244, 244);
const DARK_COMPOSER_BACKGROUND: vt100::Color = vt100::Color::Rgb(30, 30, 30);

#[test]
fn focus_gained_refreshes_palette_and_preserves_input() -> Result<()> {
    let repo_root = codex_utils_cargo_bin::repo_root()?;
    let codex_home = tempfile::tempdir()?;
    write_test_config(codex_home.path(), &repo_root)?;

    let mut terminal = PtyCodex::start(&repo_root, codex_home)?;
    terminal.wait_for_startup()?;

    let focus_started = Instant::now();
    let focus_output_start = terminal.output.len();
    terminal.write_input(format!("\u{1b}[I{FOCUS_PROBE_INPUT}").as_bytes())?;
    terminal.wait_for_palette_query(focus_started, focus_output_start)?;
    terminal.write_input(DARK_PALETTE_RESPONSE)?;
    terminal.wait_for_focus_result(FOCUS_PROBE_INPUT, DARK_COMPOSER_BACKGROUND, focus_started)?;

    let delayed_input = format!("{FOCUS_PROBE_INPUT}-delayed");
    let delayed_focus_started = Instant::now();
    let delayed_focus_output_start = terminal.output.len();
    terminal.write_input(b"\x1b[I")?;
    terminal.wait_for_palette_query(delayed_focus_started, delayed_focus_output_start)?;
    terminal.write_input(delayed_input.as_bytes())?;
    terminal.write_input(DARK_PALETTE_RESPONSE)?;
    terminal.wait_for_focus_result(
        &delayed_input,
        DARK_COMPOSER_BACKGROUND,
        delayed_focus_started,
    )?;

    Ok(())
}

#[test]
fn unanswered_focus_palette_refresh_preserves_cached_palette_and_input() -> Result<()> {
    let repo_root = codex_utils_cargo_bin::repo_root()?;
    let codex_home = tempfile::tempdir()?;
    write_test_config(codex_home.path(), &repo_root)?;

    let mut terminal = PtyCodex::start(&repo_root, codex_home)?;
    terminal.wait_for_startup()?;

    let input = format!("{FOCUS_PROBE_INPUT}-timeout");
    let focus_started = Instant::now();
    let focus_output_start = terminal.output.len();
    terminal.write_input(b"\x1b[I")?;
    terminal.wait_for_palette_query(focus_started, focus_output_start)?;
    terminal.write_input(input.as_bytes())?;
    terminal.wait_for_focus_result(&input, LIGHT_COMPOSER_BACKGROUND, focus_started)?;

    Ok(())
}

pub(super) struct PtyCodex {
    master: File,
    child: Child,
    parser: vt100::Parser,
    output: Vec<u8>,
    cursor_answered: bool,
    palette_answered: bool,
    keyboard_answered: bool,
    _codex_home: TempDir,
}

impl PtyCodex {
    pub(super) fn start(repo_root: &Path, codex_home: TempDir) -> Result<Self> {
        let mut master_fd = -1;
        let mut slave_fd = -1;
        let mut window_size = libc::winsize {
            ws_row: 32,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        // SAFETY: `openpty` initializes both file descriptors on success, and the supplied window
        // size remains valid for the duration of the call.
        let result = unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                /*name*/ std::ptr::null_mut(),
                /*termp*/ std::ptr::null_mut(),
                &raw mut window_size,
            )
        };
        if result == -1 {
            return Err(std::io::Error::last_os_error()).context("open focus-test pseudo-terminal");
        }

        // SAFETY: a successful `openpty` transfers ownership of both unique file descriptors.
        let master = File::from(unsafe { OwnedFd::from_raw_fd(master_fd) });
        // SAFETY: `slave_fd` is the second unique descriptor initialized by `openpty`.
        let slave = File::from(unsafe { OwnedFd::from_raw_fd(slave_fd) });
        let stdin = slave.try_clone().context("clone pseudo-terminal stdin")?;
        let stdout = slave.try_clone().context("clone pseudo-terminal stdout")?;

        let codex = codex_utils_cargo_bin::cargo_bin("codex-tui")
            .or_else(|_| codex_utils_cargo_bin::cargo_bin("codex"))?;
        let child = Command::new(codex)
            .arg("--no-alt-screen")
            .arg("-C")
            .arg(repo_root)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env_remove("NO_COLOR")
            .env_remove("FORCE_COLOR")
            .env("OPENAI_API_KEY", "focus-palette-test")
            .env("CODEX_HOME", codex_home.path())
            .stdin(stdin)
            .stdout(stdout)
            .stderr(slave)
            .spawn()
            .context("start Codex in focus-test pseudo-terminal")?;

        Ok(Self {
            master,
            child,
            parser: vt100::Parser::new(
                /*rows*/ 32, /*cols*/ 120, /*scrollback_len*/ 0,
            ),
            output: Vec::new(),
            cursor_answered: false,
            palette_answered: false,
            keyboard_answered: false,
            _codex_home: codex_home,
        })
    }

    pub(super) fn wait_for_startup(&mut self) -> Result<()> {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while Instant::now() < deadline {
            self.read_output(Duration::from_millis(/*millis*/ 50))?;
            self.answer_startup_queries()?;

            if self.palette_answered
                && self.screen_contains("OpenAI Codex")
                && self.composer_background() == Some(LIGHT_COMPOSER_BACKGROUND)
            {
                return Ok(());
            }

            if let Some(status) = self.child.try_wait()? {
                bail!(
                    "Codex exited before the focus test started ({status}); screen:\n{}",
                    self.screen_contents(),
                );
            }
        }

        bail!(
            "Codex did not initialize within {:?}; screen:\n{}",
            STARTUP_TIMEOUT,
            self.screen_contents(),
        );
    }

    fn wait_for_palette_query(
        &mut self,
        focus_started: Instant,
        output_start: usize,
    ) -> Result<()> {
        while focus_started.elapsed() < FOCUS_INPUT_TIMEOUT {
            self.read_output(Duration::from_millis(/*millis*/ 5))?;
            let focus_output = &self.output[output_start..];
            if contains_bytes(focus_output, b"\x1b]10;?")
                && contains_bytes(focus_output, b"\x1b]11;?")
            {
                return Ok(());
            }
        }

        bail!(
            "focus regain did not query the terminal palette within {:?}; screen:\n{}",
            FOCUS_INPUT_TIMEOUT,
            self.screen_contents(),
        );
    }

    fn wait_for_focus_result(
        &mut self,
        input: &str,
        expected_background: vt100::Color,
        focus_started: Instant,
    ) -> Result<()> {
        while focus_started.elapsed() < FOCUS_INPUT_TIMEOUT {
            self.read_output(Duration::from_millis(/*millis*/ 20))?;
            if self.screen_contains(input)
                && self.composer_background() == Some(expected_background)
            {
                return Ok(());
            }
        }

        bail!(
            "focus-time palette refresh did not preserve {input:?} with background \
             {expected_background:?} within {:?}; actual background: {:?}; screen:\n{}",
            FOCUS_INPUT_TIMEOUT,
            self.composer_background(),
            self.screen_contents(),
        );
    }

    fn answer_startup_queries(&mut self) -> Result<()> {
        if !self.cursor_answered && contains_bytes(&self.output, b"\x1b[6n") {
            self.write_input(b"\x1b[1;1R")?;
            self.cursor_answered = true;
        }

        if !self.keyboard_answered && contains_bytes(&self.output, b"\x1b[?u") {
            self.write_input(b"\x1b[?0u\x1b[?1;2c")?;
            self.keyboard_answered = true;
        }

        if !self.palette_answered
            && contains_bytes(&self.output, b"\x1b]10;?")
            && contains_bytes(&self.output, b"\x1b]11;?")
        {
            self.write_input(LIGHT_PALETTE_RESPONSE)?;
            self.palette_answered = true;
        }

        Ok(())
    }

    pub(super) fn read_output(&mut self, timeout: Duration) -> Result<()> {
        let timeout_ms = timeout.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
        let mut descriptor = libc::pollfd {
            fd: self.master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };

        // SAFETY: `descriptor` points to one initialized poll descriptor.
        let ready = unsafe {
            libc::poll(&mut descriptor, /*nfds*/ 1, timeout_ms)
        };
        if ready == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                return Ok(());
            }
            return Err(error).context("poll focus-test pseudo-terminal");
        }
        if ready == 0 || descriptor.revents & libc::POLLIN == 0 {
            return Ok(());
        }

        let mut chunk = [0_u8; 8192];
        let count = self.master.read(&mut chunk)?;
        self.output.extend_from_slice(&chunk[..count]);
        self.parser.process(&chunk[..count]);
        Ok(())
    }

    pub(super) fn write_input(&mut self, bytes: &[u8]) -> Result<()> {
        self.master.write_all(bytes)?;
        self.master.flush()?;
        Ok(())
    }

    pub(super) fn screen_contains(&self, text: &str) -> bool {
        self.parser.screen().contents().contains(text)
    }

    pub(super) fn screen_contents(&self) -> String {
        self.parser.screen().contents()
    }

    fn composer_background(&self) -> Option<vt100::Color> {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        (0..rows).rev().find_map(|row| {
            (0..cols).find_map(|col| {
                let cell = screen.cell(row, col)?;
                (cell.bgcolor() != vt100::Color::Default).then(|| cell.bgcolor())
            })
        })
    }
}

impl Drop for PtyCodex {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn contains_bytes(buffer: &[u8], needle: &[u8]) -> bool {
    buffer.windows(needle.len()).any(|window| window == needle)
}

pub(super) fn write_test_config(codex_home: &Path, repo_root: &Path) -> Result<()> {
    let repo_root = repo_root.display();
    let config = format!(
        "model = \"gpt-5.6-terra\"\nmodel_provider = \"openai\"\n\
         suppress_unstable_features_warning = true\nanalytics.enabled = false\n\n\
         [projects.\"{repo_root}\"]\ntrust_level = \"trusted\"\n"
    );
    std::fs::write(codex_home.join("config.toml"), config)
        .context("write focus-test Codex configuration")?;
    std::fs::write(
        codex_home.join("auth.json"),
        r#"{"OPENAI_API_KEY":"focus-palette-test","tokens":null,"last_refresh":null}"#,
    )
    .context("write focus-test API-key authentication")
}
