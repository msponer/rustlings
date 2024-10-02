use anyhow::{Context, Result};
use crossterm::{
    style::{
        Attribute, Attributes, Color, ResetColor, SetAttribute, SetAttributes, SetForegroundColor,
    },
    terminal, QueueableCommand,
};
use std::{
    io::{self, Read, StdoutLock, Write},
    sync::mpsc::{sync_channel, Sender, SyncSender},
    thread,
};

use crate::{
    app_state::{AppState, ExercisesProgress},
    clear_terminal,
    exercise::{solution_link_line, RunnableExercise, OUTPUT_CAPACITY},
    term::progress_bar,
};

use super::{terminal_event::terminal_event_handler, InputPauseGuard, WatchEvent};

#[derive(PartialEq, Eq)]
enum DoneStatus {
    DoneWithSolution(String),
    DoneWithoutSolution,
    Pending,
}

pub struct WatchState<'a> {
    app_state: &'a mut AppState,
    output: Vec<u8>,
    show_hint: bool,
    done_status: DoneStatus,
    manual_run: bool,
    term_width: u16,
    terminal_event_unpause_sender: SyncSender<()>,
}

impl<'a> WatchState<'a> {
    pub fn build(
        app_state: &'a mut AppState,
        watch_event_sender: Sender<WatchEvent>,
        manual_run: bool,
    ) -> Result<Self> {
        let term_width = terminal::size()
            .context("Failed to get the terminal size")?
            .0;

        let (terminal_event_unpause_sender, terminal_event_unpause_receiver) = sync_channel(0);

        thread::Builder::new()
            .spawn(move || {
                terminal_event_handler(
                    watch_event_sender,
                    terminal_event_unpause_receiver,
                    manual_run,
                )
            })
            .context("Failed to spawn a thread to handle terminal events")?;

        Ok(Self {
            app_state,
            output: Vec::with_capacity(OUTPUT_CAPACITY),
            show_hint: false,
            done_status: DoneStatus::Pending,
            manual_run,
            term_width,
            terminal_event_unpause_sender,
        })
    }

    pub fn run_current_exercise(&mut self, stdout: &mut StdoutLock) -> Result<()> {
        // Ignore any input until running the exercise is done.
        let _input_pause_guard = InputPauseGuard::scoped_pause();

        self.show_hint = false;

        writeln!(
            stdout,
            "\nChecking the exercise `{}`. Please wait…",
            self.app_state.current_exercise().name,
        )?;

        let success = self
            .app_state
            .current_exercise()
            .run_exercise(Some(&mut self.output), self.app_state.cmd_runner())?;
        self.output.push(b'\n');
        if success {
            self.done_status =
                if let Some(solution_path) = self.app_state.current_solution_path()? {
                    DoneStatus::DoneWithSolution(solution_path)
                } else {
                    DoneStatus::DoneWithoutSolution
                };
        } else {
            self.app_state
                .set_pending(self.app_state.current_exercise_ind())?;

            self.done_status = DoneStatus::Pending;
        }

        self.render(stdout)?;
        Ok(())
    }

    pub fn reset_exercise(&mut self, stdout: &mut StdoutLock) -> Result<()> {
        clear_terminal(stdout)?;

        stdout.write_all(b"Resetting will undo all your changes to the file ")?;
        stdout.write_all(self.app_state.current_exercise().path.as_bytes())?;
        stdout.write_all(b"\nReset (y/n)? ")?;
        stdout.flush()?;

        {
            let mut stdin = io::stdin().lock();
            let mut answer = [0];
            loop {
                stdin
                    .read_exact(&mut answer)
                    .context("Failed to read the user's input")?;

                match answer[0] {
                    b'y' | b'Y' => {
                        self.app_state.reset_current_exercise()?;

                        // The file watcher reruns the exercise otherwise.
                        if self.manual_run {
                            self.run_current_exercise(stdout)?;
                        }
                    }
                    b'n' | b'N' => self.render(stdout)?,
                    _ => continue,
                }

                break;
            }
        }

        self.terminal_event_unpause_sender.send(())?;

        Ok(())
    }

    pub fn handle_file_change(
        &mut self,
        exercise_ind: usize,
        stdout: &mut StdoutLock,
    ) -> Result<()> {
        if self.app_state.current_exercise_ind() != exercise_ind {
            return Ok(());
        }

        self.run_current_exercise(stdout)
    }

    /// Move on to the next exercise if the current one is done.
    pub fn next_exercise(&mut self, stdout: &mut StdoutLock) -> Result<ExercisesProgress> {
        if self.done_status == DoneStatus::Pending {
            return Ok(ExercisesProgress::CurrentPending);
        }

        self.app_state.done_current_exercise::<true>(stdout)
    }

    fn show_prompt(&self, stdout: &mut StdoutLock) -> io::Result<()> {
        if self.done_status != DoneStatus::Pending {
            stdout.queue(SetAttribute(Attribute::Bold))?;
            stdout.write_all(b"n")?;
            stdout.queue(ResetColor)?;
            stdout.write_all(b":")?;
            stdout.queue(SetAttribute(Attribute::Underlined))?;
            stdout.write_all(b"next")?;
            stdout.queue(ResetColor)?;
            stdout.write_all(b" / ")?;
        }

        if self.manual_run {
            stdout.queue(SetAttribute(Attribute::Bold))?;
            stdout.write_all(b"r")?;
            stdout.queue(ResetColor)?;
            stdout.write_all(b":run / ")?;
        }

        if !self.show_hint {
            stdout.queue(SetAttribute(Attribute::Bold))?;
            stdout.write_all(b"h")?;
            stdout.queue(ResetColor)?;
            stdout.write_all(b":hint / ")?;
        }

        stdout.queue(SetAttribute(Attribute::Bold))?;
        stdout.write_all(b"l")?;
        stdout.queue(ResetColor)?;
        stdout.write_all(b":list / ")?;

        stdout.queue(SetAttribute(Attribute::Bold))?;
        stdout.write_all(b"c")?;
        stdout.queue(ResetColor)?;
        stdout.write_all(b":check all / ")?;

        stdout.queue(SetAttribute(Attribute::Bold))?;
        stdout.write_all(b"x")?;
        stdout.queue(ResetColor)?;
        stdout.write_all(b":reset / ")?;

        stdout.queue(SetAttribute(Attribute::Bold))?;
        stdout.write_all(b"q")?;
        stdout.queue(ResetColor)?;
        stdout.write_all(b":quit ? ")?;

        stdout.flush()
    }

    pub fn render(&self, stdout: &mut StdoutLock) -> io::Result<()> {
        // Prevent having the first line shifted if clearing wasn't successful.
        stdout.write_all(b"\n")?;
        clear_terminal(stdout)?;

        stdout.write_all(&self.output)?;

        if self.show_hint {
            stdout
                .queue(SetAttributes(
                    Attributes::from(Attribute::Bold).with(Attribute::Underlined),
                ))?
                .queue(SetForegroundColor(Color::Cyan))?;
            stdout.write_all(b"Hint")?;
            stdout.queue(ResetColor)?;
            stdout.write_all(b"\n")?;

            stdout.write_all(self.app_state.current_exercise().hint.as_bytes())?;
            stdout.write_all(b"\n\n")?;
        }

        if self.done_status != DoneStatus::Pending {
            stdout
                .queue(SetAttribute(Attribute::Bold))?
                .queue(SetForegroundColor(Color::Green))?;
            stdout.write_all("Exercise done ✓".as_bytes())?;
            stdout.queue(ResetColor)?;
            stdout.write_all(b"\n")?;

            if let DoneStatus::DoneWithSolution(solution_path) = &self.done_status {
                solution_link_line(stdout, solution_path)?;
            }

            stdout.write_all(
                "When done experimenting, enter `n` to move on to the next exercise 🦀\n\n"
                    .as_bytes(),
            )?;
        }

        progress_bar(
            stdout,
            self.app_state.n_done(),
            self.app_state.exercises().len() as u16,
            self.term_width,
        )?;

        stdout.write_all(b"\nCurrent exercise: ")?;
        self.app_state
            .current_exercise()
            .terminal_file_link(stdout)?;
        stdout.write_all(b"\n\n")?;

        self.show_prompt(stdout)?;

        Ok(())
    }

    pub fn show_hint(&mut self, stdout: &mut StdoutLock) -> io::Result<()> {
        if !self.show_hint {
            self.show_hint = true;
            self.render(stdout)?;
        }

        Ok(())
    }

    pub fn check_all_exercises(&mut self, stdout: &mut StdoutLock) -> Result<ExercisesProgress> {
        stdout.write_all(b"\n")?;

        if let Some(first_fail) = self.app_state.check_all_exercises(stdout, false)? {
            // Only change exercise if the current one is done...
            if self.app_state.current_exercise().done {
                self.app_state.set_current_exercise_ind(first_fail)?;
            }
            // ...but always pretend it's a "new" anyway because that refreshes
            // the display
            Ok(ExercisesProgress::NewPending)
        } else {
            self.app_state.render_final_message(stdout)?;
            Ok(ExercisesProgress::AllDone)
        }
    }

    pub fn update_term_width(&mut self, width: u16, stdout: &mut StdoutLock) -> io::Result<()> {
        if self.term_width != width {
            self.term_width = width;
            self.render(stdout)?;
        }

        Ok(())
    }
}
