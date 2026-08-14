//! A module for working with processes.

use crate::wit::zed::extension::process;
pub use crate::wit::zed::extension::process::{Command, Output, ScopedCommand};

impl Command {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            command: program.into(),
            args: Vec::new(),
            env: Vec::new(),
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn envs(
        mut self,
        envs: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Self {
        self.env.extend(
            envs.into_iter()
                .map(|(key, value)| (key.into(), value.into())),
        );
        self
    }

    pub fn output(&mut self) -> Result<Output, String> {
        process::run_command(self)
    }
}

impl ScopedCommand {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            command: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            working_directory: String::new(),
            stdin: None,
            redact_stdin: false,
        }
    }

    pub fn arg(mut self, argument: impl Into<String>) -> Self {
        self.args.push(argument.into());
        self
    }

    pub fn args(mut self, arguments: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args.extend(arguments.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn envs(
        mut self,
        environment: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Self {
        self.env.extend(
            environment
                .into_iter()
                .map(|(key, value)| (key.into(), value.into())),
        );
        self
    }

    pub fn current_dir(mut self, path: impl Into<String>) -> Self {
        self.working_directory = path.into();
        self
    }

    pub fn stdin(mut self, stdin: impl Into<Vec<u8>>, redact: bool) -> Self {
        self.stdin = Some(stdin.into());
        self.redact_stdin = redact;
        self
    }
}
