use std::borrow::Cow;

/// Controls how the process name is automatically generated when not explicitly provided.
///
/// This determines what information is included in the auto-generated process name
/// used in public errors, logs, and debugging output. The default captures only
/// the program name so command arguments and environment variables are not logged accidentally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoName {
    /// Capture a name from the command as specified by the provided settings.
    ///
    /// Example: `ls "-la"` from `Command::new("ls").arg("-la")` when using
    /// [`AutoNameSettings::program_with_args`].
    Using(AutoNameSettings),

    /// Capture the full Debug representation of the Command.
    ///
    /// Example: `"Command { std: \"ls\" \"-la\", kill_on_drop: false }"`
    ///
    /// Note: This includes internal implementation details and may change with tokio updates.
    /// It may also include command arguments, environment variables, or other sensitive data.
    Debug,
}

impl Default for AutoName {
    fn default() -> Self {
        Self::Using(AutoNameSettings::program_only())
    }
}

/// Controls in detail which parts of the command are automatically captured as the process name.
///
/// Process names are used in public error messages and tracing fields. Only capture
/// arguments, environment variables, or current directories when those values are safe to log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag controls one optional part of the generated process name"
)]
pub struct AutoNameSettings {
    include_current_dir: bool,
    include_envs: bool,
    include_program: bool,
    include_args: bool,
}

impl AutoNameSettings {
    /// Capture the program name.
    ///
    /// Example: `Command::new("ls").arg("-la").env("FOO", "foo)` is captured as `"ls"`.
    #[must_use]
    pub fn program_only() -> Self {
        AutoNameSettings {
            include_current_dir: false,
            include_envs: false,
            include_program: true,
            include_args: false,
        }
    }

    /// Capture the program name and all arguments.
    ///
    /// Example: `Command::new("ls").arg("-la").env("FOO", "foo)` is captured as `ls "-la"`.
    ///
    /// Arguments may contain tokens, passwords, signed URLs, or headers. Use this only when
    /// arguments are safe to include in public errors and logs or for debugging purposes.
    #[must_use]
    pub fn program_with_args() -> Self {
        AutoNameSettings {
            include_current_dir: false,
            include_envs: false,
            include_program: true,
            include_args: true,
        }
    }

    /// Capture the program name and all environment variables and arguments.
    ///
    /// Example: `Command::new("ls").arg("-la").env("FOO", "foo)` is captured as `FOO=foo ls "-la"`.
    ///
    /// Environment variables and arguments often contain credentials. Use this only when all
    /// captured values are safe to include in public errors and logs or for debugging purposes.
    #[must_use]
    pub fn program_with_env_and_args() -> Self {
        AutoNameSettings {
            include_current_dir: false,
            include_envs: true,
            include_program: true,
            include_args: true,
        }
    }

    /// Capture the directory and the program name and all environment variables and arguments.
    ///
    /// Example: `Command::new("ls").arg("-la").env("FOO", "foo)` is captured as
    /// `/some/dir % FOO=foo ls "-la"`.
    ///
    /// Current directories, environment variables, and arguments may contain sensitive data.
    /// Use this only when all captured values are safe to include in public errors and logs.
    #[must_use]
    pub fn full() -> Self {
        AutoNameSettings {
            include_current_dir: true,
            include_envs: true,
            include_program: true,
            include_args: true,
        }
    }

    fn format_cmd(self, cmd: &std::process::Command) -> String {
        let mut name = String::new();
        if self.include_current_dir
            && let Some(current_dir) = cmd.get_current_dir()
        {
            name.push_str(current_dir.to_string_lossy().as_ref());
            name.push_str(" % ");
        }
        if self.include_envs {
            let envs = cmd.get_envs();
            if envs.len() != 0 {
                for (key, value) in envs
                    .filter(|(_key, value)| value.is_some())
                    .map(|(key, value)| (key, value.expect("present")))
                {
                    name.push_str(key.to_string_lossy().as_ref());
                    name.push('=');
                    name.push_str(value.to_string_lossy().as_ref());
                    name.push(' ');
                }
            }
        }
        if self.include_program {
            name.push_str(cmd.get_program().to_string_lossy().as_ref());
            name.push(' ');
        }
        if self.include_args {
            let args = cmd.get_args();
            if args.len() != 0 {
                for arg in args {
                    name.push('"');
                    name.push_str(arg.to_string_lossy().as_ref());
                    name.push('"');
                    name.push(' ');
                }
            }
        }
        if name.ends_with(' ') {
            name.pop();
        }
        name
    }
}

/// Specifies how a process should be named.
///
/// This enum allows you to either provide an explicit name or configure automatic
/// name generation. Using this type ensures you cannot accidentally set both an
/// explicit name and an auto-naming mode at the same time.
///
/// Process names are used in public error messages and tracing fields. Prefer
/// [`ProcessName::Explicit`] for stable safe labels when commands may contain secrets.
#[derive(Debug, Clone)]
pub enum ProcessName {
    /// Use an explicit custom name.
    ///
    /// Example: `ProcessName::Explicit("my-server")`
    Explicit(Cow<'static, str>),

    /// Auto-generate the name based on the command.
    ///
    /// The default automatic mode captures only the program name. Use argument,
    /// environment, or debug capture only when those values are safe to log.
    Auto(AutoName),
}

impl Default for ProcessName {
    fn default() -> Self {
        Self::Auto(AutoName::default())
    }
}

impl From<&'static str> for ProcessName {
    fn from(s: &'static str) -> Self {
        Self::Explicit(Cow::Borrowed(s))
    }
}

impl From<String> for ProcessName {
    fn from(s: String) -> Self {
        Self::Explicit(Cow::Owned(s))
    }
}

impl From<Cow<'static, str>> for ProcessName {
    fn from(s: Cow<'static, str>) -> Self {
        Self::Explicit(s)
    }
}

impl From<AutoName> for ProcessName {
    fn from(mode: AutoName) -> Self {
        Self::Auto(mode)
    }
}

pub(super) fn generate_name(
    name: &ProcessName,
    cmd: &tokio::process::Command,
) -> Cow<'static, str> {
    match name {
        ProcessName::Explicit(name) => name.clone(),
        ProcessName::Auto(auto_name) => match auto_name {
            AutoName::Using(settings) => settings.format_cmd(cmd.as_std()).into(),
            AutoName::Debug => format!("{cmd:?}").into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assertr::prelude::*;
    use std::path::PathBuf;
    use tokio::process::Command;

    fn command_with_args_env_and_current_dir() -> Command {
        let mut cmd = Command::new("ls");
        cmd.arg("-la");
        cmd.env("FOO", "foo");
        cmd.current_dir(PathBuf::from("./"));
        cmd
    }

    #[test]
    fn auto_name_captures_only_program_by_default() {
        let mut cmd = command_with_args_env_and_current_dir();
        let sensitive_arg = "--token=secret-token-should-not-be-logged";
        cmd.arg(sensitive_arg);

        let name = generate_name(&ProcessName::default(), &cmd);

        assert_that!(name.as_ref()).is_equal_to("ls");
        assert_that!(name.as_ref()).does_not_contain(sensitive_arg);
    }

    #[test]
    fn auto_name_only_captures_command_when_requested() {
        let cmd = command_with_args_env_and_current_dir();

        let name = generate_name(
            &ProcessName::Auto(AutoName::Using(AutoNameSettings::program_only())),
            &cmd,
        );

        assert_that!(name.as_ref()).is_equal_to("ls");
    }

    #[test]
    fn auto_name_captures_command_with_args_when_requested() {
        let cmd = command_with_args_env_and_current_dir();

        let name = generate_name(
            &ProcessName::Auto(AutoName::Using(AutoNameSettings::program_with_args())),
            &cmd,
        );

        assert_that!(name.as_ref()).is_equal_to("ls \"-la\"");
    }

    #[test]
    fn auto_name_captures_command_with_envs_and_args_when_requested() {
        let cmd = command_with_args_env_and_current_dir();

        let name = generate_name(
            &ProcessName::Auto(AutoName::Using(
                AutoNameSettings::program_with_env_and_args(),
            )),
            &cmd,
        );

        assert_that!(name.as_ref()).is_equal_to("FOO=foo ls \"-la\"");
    }

    #[test]
    fn auto_name_captures_command_with_current_dir_envs_and_args_when_requested() {
        let cmd = command_with_args_env_and_current_dir();

        let name = generate_name(
            &ProcessName::Auto(AutoName::Using(AutoNameSettings::full())),
            &cmd,
        );

        assert_that!(name.as_ref()).is_equal_to("./ % FOO=foo ls \"-la\"");
    }

    #[test]
    fn auto_name_captures_full_command_debug_string_when_requested() {
        let cmd = command_with_args_env_and_current_dir();

        let name = generate_name(&ProcessName::Auto(AutoName::Debug), &cmd);

        assert_that!(name.as_ref()).is_equal_to(
            "Command { std: cd \"./\" && FOO=\"foo\" \"ls\" \"-la\", kill_on_drop: false }",
        );
    }

    #[test]
    fn custom_name_uses_explicit_label() {
        let id = 42;
        let cmd = Command::new("ls");

        let name = generate_name(&ProcessName::from(format!("worker-{id}")), &cmd);

        assert_that!(name.as_ref()).is_equal_to("worker-42");
    }
}
