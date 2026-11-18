//! Command blocks from OSC 133 semantic prompt marks: each block is one
//! prompt, the command typed at it, the command's output and exit status.
//! Shells emit the marks themselves (fish) or through a small hook (`jetmux
//! shell-init`). [`VtCore::write`] records each mark where it falls, once
//! [`VtCore::track_blocks`] asks it to.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::Error;
use crate::screen::{Anchor, ScreenPos};
use crate::vt::VtCore;

/// Blocks kept per pane. Every block holds tracked grid refs, which cost a
/// little on every terminal mutation.
const MAX_BLOCKS: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// `A`: a prompt starts.
    Prompt,
    /// `B`: the prompt ends and the user's input starts.
    Input,
    /// `C`: the command runs; its output starts.
    Output,
    /// `D[;status]`: the command finished.
    Done(Option<i32>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockState {
    /// At the prompt; nothing has run yet.
    Prompt,
    Running,
    Done,
}

#[derive(Debug)]
pub struct Block {
    pub id: u64,
    prompt: Anchor,
    input: Option<Anchor>,
    output: Option<Anchor>,
    end: Option<Anchor>,
    /// What was typed at the prompt, once the command started.
    pub command: String,
    pub exit: Option<i32>,
    started: Option<Instant>,
    pub duration: Option<Duration>,
}

impl Block {
    pub fn state(&self) -> BlockState {
        match (&self.output, &self.end) {
            (None, _) => BlockState::Prompt,
            (Some(_), None) => BlockState::Running,
            (Some(_), Some(_)) => BlockState::Done,
        }
    }

    /// How long the command has run so far, or ran.
    pub fn elapsed(&self) -> Option<Duration> {
        self.duration.or_else(|| self.started.map(|s| s.elapsed()))
    }

    pub fn failed(&self) -> bool {
        self.exit.is_some_and(|e| e != 0)
    }
}

/// Where a block sits in the screen, for copy mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub prompt: ScreenPos,
    pub output: Option<ScreenPos>,
    pub end: Option<ScreenPos>,
    pub failed: bool,
}

#[derive(Debug, Default)]
pub struct Blocks {
    list: VecDeque<Block>,
    next_id: u64,
    /// Blocks that finished since the last [`Blocks::take_finished`].
    finished: Vec<u64>,
}

impl Blocks {
    /// Record `mark` at the terminal cursor. Marks on the alternate screen
    /// are ignored: blocks live in the primary screen's history.
    pub(crate) fn mark(&mut self, vt: &VtCore, mark: Mark) -> Result<(), Error> {
        let Some(here) = vt.anchor_cursor()? else {
            return Ok(());
        };
        let at_prompt = self
            .list
            .back()
            .is_some_and(|b| b.state() == BlockState::Prompt);
        match mark {
            Mark::Prompt => {
                // A shell that never reports `D` still ends its block here.
                self.finish(vt, None)?;
                self.push(here);
            }
            Mark::Input if at_prompt => {
                if let Some(block) = self.list.back_mut() {
                    block.input = Some(here);
                }
            }
            Mark::Input => {
                self.push(here);
                if let Some(block) = self.list.back_mut() {
                    block.input = vt.anchor_cursor()?;
                }
            }
            Mark::Output => {
                if !at_prompt {
                    self.push(here);
                }
                if let Some(block) = self.list.back_mut() {
                    let typed = block.input.as_ref().unwrap_or(&block.prompt);
                    if let (Some(from), Ok(to)) = (vt.anchor_pos(typed), vt.screen_cursor()) {
                        block.command = vt.text_between(from, to)?.trim().to_owned();
                    }
                    block.output = vt.anchor_cursor()?;
                    block.started = Some(Instant::now());
                }
            }
            Mark::Done(exit) => self.finish(vt, exit)?,
        }
        while self.list.len() > MAX_BLOCKS
            || self
                .list
                .front()
                .is_some_and(|b| vt.anchor_pos(&b.prompt).is_none() && self.list.len() > 1)
        {
            self.list.pop_front();
        }
        Ok(())
    }

    fn push(&mut self, prompt: Anchor) {
        self.next_id += 1;
        self.list.push_back(Block {
            id: self.next_id,
            prompt,
            input: None,
            output: None,
            end: None,
            command: String::new(),
            exit: None,
            started: None,
            duration: None,
        });
    }

    /// End the running block, if there is one.
    fn finish(&mut self, vt: &VtCore, exit: Option<i32>) -> Result<(), Error> {
        let Some(block) = self.list.back_mut() else {
            return Ok(());
        };
        if block.state() != BlockState::Running {
            return Ok(());
        }
        block.end = vt.anchor_cursor()?;
        block.exit = exit;
        block.duration = block.started.map(|s| s.elapsed());
        self.finished.push(block.id);
        Ok(())
    }

    pub(crate) fn take_finished(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.finished)
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Block> {
        self.list.iter()
    }

    pub fn get(&self, id: u64) -> Option<&Block> {
        self.list.iter().find(|b| b.id == id)
    }

    /// The newest block that has finished.
    pub fn last_done(&self) -> Option<&Block> {
        self.list
            .iter()
            .rev()
            .find(|b| b.state() == BlockState::Done)
    }

    /// A block's output as plain text; a running block's output so far.
    pub fn output(&self, vt: &VtCore, block: &Block) -> Result<Option<String>, Error> {
        let Some(start) = block.output.as_ref().and_then(|a| vt.anchor_pos(a)) else {
            return Ok(None);
        };
        let end = match block.end.as_ref() {
            Some(end) => vt.anchor_pos(end),
            None => vt.screen_cursor().ok(),
        };
        let Some(end) = end else {
            return Ok(None);
        };
        Ok(Some(vt.text_between(start, end)?))
    }

    pub fn spans(&self, vt: &VtCore) -> Vec<Span> {
        self.list
            .iter()
            .filter_map(|b| {
                Some(Span {
                    prompt: vt.anchor_pos(&b.prompt)?,
                    output: b.output.as_ref().and_then(|a| vt.anchor_pos(a)),
                    end: b.end.as_ref().and_then(|a| vt.anchor_pos(a)),
                    failed: b.failed(),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::blocks::BlockState;
    use crate::vt::VtCore;

    #[test]
    fn blocks_hold_command_output_and_status() -> Result<(), crate::Error> {
        let mut vt = VtCore::new(20, 4, 100)?;
        vt.track_blocks();
        let prompt = |cmd: &str| format!("\x1b]133;A\x07$ \x1b]133;B\x07{cmd}\r\n\x1b]133;C\x07");
        let mut out = prompt("ls");
        out.push_str("a\r\nb\r\n\x1b]133;D;0\x07");
        out.push_str(&prompt("false"));
        out.push_str("nope\r\n\x1b]133;D;1\x1b\\\x1b]133;A\x07$ ");
        // A mark split across writes still lands where it ends.
        let (head, tail) = out.split_at(out.find("133;D;1").unwrap_or_default() + 3);
        vt.write(head.as_bytes());
        vt.write(tail.as_bytes());

        let finished = vt.take_finished_blocks();
        let blocks = vt.blocks();
        let all: Vec<_> = blocks.iter().collect();
        assert_eq!(all.len(), 3);
        assert_eq!((all[0].command.as_str(), all[0].exit), ("ls", Some(0)));
        assert_eq!(blocks.output(&vt, all[0])?.as_deref(), Some("a\nb"));
        assert_eq!((all[1].command.as_str(), all[1].exit), ("false", Some(1)));
        assert_eq!(blocks.output(&vt, all[1])?.as_deref(), Some("nope"));
        assert_eq!(all[2].state(), BlockState::Prompt);
        assert_eq!(finished, [all[0].id, all[1].id]);
        // The anchors follow their rows as the block scrolls off screen.
        let spans = blocks.spans(&vt);
        assert!(spans[1].failed && !spans[0].failed);
        assert_eq!(spans[0].prompt.y + 3, spans[1].prompt.y);
        Ok(())
    }

    #[test]
    fn untracked_terminals_keep_no_blocks() -> Result<(), crate::Error> {
        let mut vt = VtCore::new(20, 4, 100)?;
        vt.write(b"\x1b]133;A\x07$ \x1b]133;C\x07x\r\n\x1b]133;D;0\x07");
        assert_eq!(vt.blocks().iter().count(), 0);
        assert_eq!(vt.take_effects()?.finished, [Some(0)]);
        Ok(())
    }
}
