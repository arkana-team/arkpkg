use std::io::{self, BufRead, Write};

use crate::errors::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptChoice {
    Yes,
    No,
    YesToAll,
    NoToAll,
}

/// Manages interactive user prompts and non-interactive flag overrides.
#[derive(Debug, Clone)]
pub struct PromptHandler {
    auto_yes: bool,
    auto_no: bool,
    force: bool,
    global_choice: Option<PromptChoice>,
}

impl PromptHandler {
    pub fn new(auto_yes: bool, auto_no: bool, force: bool) -> Self {
        Self {
            auto_yes,
            auto_no,
            force,
            global_choice: None,
        }
    }

    /// Asks a prompt question and returns true if action is confirmed (Yes / Yes to All), false otherwise.
    pub fn ask(&mut self, message: &str) -> Result<bool> {
        if self.force || self.auto_yes {
            return Ok(true);
        }
        if self.auto_no {
            return Ok(false);
        }

        if let Some(choice) = self.global_choice {
            return match choice {
                PromptChoice::Yes | PromptChoice::YesToAll => Ok(true),
                PromptChoice::No | PromptChoice::NoToAll => Ok(false),
            };
        }

        println!("{}", message);
        println!("[Y] Yes");
        println!("[N] No");
        println!("[A] Yes to All");
        println!("[S] No to All");
        print!("Selection [Y/N/A/S]: ");
        io::stdout().flush()?;

        let stdin = io::stdin();
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;

        let choice = match line.trim().to_uppercase().as_str() {
            "Y" | "YES" => PromptChoice::Yes,
            "N" | "NO" => PromptChoice::No,
            "A" | "ALL" | "YES TO ALL" => {
                self.global_choice = Some(PromptChoice::YesToAll);
                PromptChoice::YesToAll
            }
            "S" | "NONE" | "NO TO ALL" => {
                self.global_choice = Some(PromptChoice::NoToAll);
                PromptChoice::NoToAll
            }
            _ => {
                println!("Invalid selection, defaulting to No.");
                PromptChoice::No
            }
        };

        match choice {
            PromptChoice::Yes | PromptChoice::YesToAll => Ok(true),
            PromptChoice::No | PromptChoice::NoToAll => Ok(false),
        }
    }
}
