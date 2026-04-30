use std::borrow::Cow;
use typed_builder::TypedBuilder;

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
    /// [`AutoName::program_with_args`].
    Using(AutoNameSettings),

    /// Capture the full Debug representation of the Command.
    ///
    /// Example: `"Command { std: \"ls\" \"-la\", kill_on_drop: false }"`
    ///
    /// Note: This includes internal implementation details and may change with tokio updates.
    /// It may also include command arguments, environment variables, or other sensitive data.
    Debug,
}

impl AutoName {
    /// Capture only the program name.
    ///
    /// This is the safe default and excludes command arguments and environment variables.
    #[must_use]
    pub fn program_only() -> Self {
        Self::Using(AutoNameSettings::program_only())
    }

    /// Capture the program name and all arguments.
    ///
    /// Arguments may contain sensitive data. Use this only when arguments are safe to log.
    #[must_use]
    pub fn program_with_args() -> Self {
        Self::Using(AutoNameSettings::program_with_args())
    }

    /// Capture the program name together with all environment variables and arguments.
    ///
    /// Environment variables and arguments often contain credentials. Use this only when all
    /// captured values are safe to log.
    #[must_use]
    pub fn program_with_env_and_args() -> Self {
        Self::Using(AutoNameSettings::program_with_env_and_args())
    }

    /// Capture the current directory together with the program name, environment variables, and arguments.
    ///
    /// Current directories, environment variables, and arguments may contain sensitive data. Use
    /// this only when all captured values are safe to log.
    #[must_use]
    pub fn full() -> Self {
        Self::Using(AutoNameSettings::full())
    }
}

impl Default for AutoName {
    fn default() -> Self {
        Self::program_only()
    }
}

/// Controls in detail which parts of the command are automatically captured as the process name.
///
/// Process names are used in public error messages and tracing fields. Only capture
/// arguments, environment variables, or current directories when those values are safe to log.
/// Use [`AutoNameSettings::builder`] to opt into custom combinations; the builder defaults to
/// capturing only the program name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, TypedBuilder)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag controls one optional part of the generated process name"
)]
pub struct AutoNameSettings {
    #[builder(default = false)]
    include_current_dir: bool,
    #[builder(default = false)]
    include_envs: bool,
    #[builder(default = true, setter(skip))]
    include_program: bool,
    #[builder(default = false)]
    include_args: bool,
}

impl Default for AutoNameSettings {
    fn default() -> Self {
        Self {
            include_current_dir: false,
            include_envs: false,
            include_program: true,
            include_args: false,
        }
    }
}

impl AutoNameSettings {
    /// Capture the program name.
    ///
    /// Example: `Command::new("ls").arg("-la").env("FOO", "foo)` is captured as `"ls"`.
    #[must_use]
    pub fn program_only() -> Self {
        Self::default()
    }

    /// Capture the program name and all arguments.
    ///
    /// Example: `Command::new("ls").arg("-la").env("FOO", "foo)` is captured as `ls "-la"`.
    ///
    /// Arguments may contain tokens, passwords, signed URLs, or headers. Use this only when
    /// arguments are safe to include in public errors and logs or for debugging purposes.
    #[must_use]
    pub fn program_with_args() -> Self {
        Self::builder().include_args(true).build()
    }

    /// Capture the program name and all environment variables and arguments.
    ///
    /// Example: `Command::new("ls").arg("-la").env("FOO", "foo)` is captured as `FOO=foo ls "-la"`.
    ///
    /// Environment variables and arguments often contain credentials. Use this only when all
    /// captured values are safe to include in public errors and logs or for debugging purposes.
    #[must_use]
    pub fn program_with_env_and_args() -> Self {
        Self::builder()
            .include_envs(true)
            .include_args(true)
            .build()
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
        Self::builder()
            .include_current_dir(true)
            .include_envs(true)
            .include_args(true)
            .build()
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
            for (key, value) in cmd
                .get_envs()
                .filter_map(|(key, value)| Some((key, value?)))
            {
                name.push_str(key.to_string_lossy().as_ref());
                name.push('=');
                name.push_str(value.to_string_lossy().as_ref());
                name.push(' ');
            }
        }
        if self.include_program {
            name.push_str(cmd.get_program().to_string_lossy().as_ref());
            name.push(' ');
        }
        if self.include_args {
            for arg in cmd.get_args() {
                name.push('"');
                name.push_str(arg.to_string_lossy().as_ref());
                name.push('"');
                name.push(' ');
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

impl From<AutoNameSettings> for AutoName {
    fn from(settings: AutoNameSettings) -> Self {
        Self::Using(settings)
    }
}

impl From<AutoNameSettings> for ProcessName {
    fn from(settings: AutoNameSettings) -> Self {
        Self::Auto(settings.into())
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
    fn auto_name_defaults_to_safe_program_only_naming() {
        let mut cmd = command_with_args_env_and_current_dir();
        let sensitive_arg = "--token=secret-token-should-not-be-logged";
        cmd.arg(sensitive_arg);

        let default_process_name = generated_name(ProcessName::default(), &cmd);
        let default_auto_name = generated_name(AutoName::default(), &cmd);
        let program_only_name = generated_name(AutoName::program_only(), &cmd);
        let builder_default_name = generated_name(AutoNameSettings::builder().build(), &cmd);

        for name in [
            default_process_name.as_str(),
            default_auto_name.as_str(),
            program_only_name.as_str(),
            builder_default_name.as_str(),
        ] {
            assert_that!(name).is_equal_to("ls");
            assert_that!(name).does_not_contain(sensitive_arg);
        }
    }

    #[test]
    fn auto_name_presets_match_settings_and_expected_output() {
        let cmd = command_with_args_env_and_current_dir();
        let cases = [
            (
                AutoName::program_only(),
                AutoNameSettings::program_only(),
                "ls",
            ),
            (
                AutoName::program_with_args(),
                AutoNameSettings::program_with_args(),
                "ls \"-la\"",
            ),
            (
                AutoName::program_with_env_and_args(),
                AutoNameSettings::program_with_env_and_args(),
                "FOO=foo ls \"-la\"",
            ),
            (
                AutoName::full(),
                AutoNameSettings::full(),
                "./ % FOO=foo ls \"-la\"",
            ),
        ];

        for (auto_name, settings, expected) in cases {
            assert_that!(auto_name).is_equal_to(AutoName::Using(settings));
            assert_that!(generated_name(auto_name, &cmd)).is_equal_to(expected);
            assert_that!(generated_name(settings, &cmd)).is_equal_to(expected);
        }
    }

    #[test]
    fn auto_name_debug_uses_command_debug_string() {
        let cmd = command_with_args_env_and_current_dir();

        assert_that!(generated_name(ProcessName::Auto(AutoName::Debug), &cmd)).is_equal_to(
            "Command { std: cd \"./\" && FOO=\"foo\" \"ls\" \"-la\", kill_on_drop: false }",
        );
    }

    #[test]
    fn auto_name_settings_builder_supports_custom_combination() {
        let cmd = command_with_args_env_and_current_dir();

        assert_that!(generated_name(
            AutoNameSettings::builder()
                .include_current_dir(true)
                .include_args(true)
                .build(),
            &cmd,
        ))
        .is_equal_to("./ % ls \"-la\"");
    }

    fn generated_name(name: impl Into<ProcessName>, cmd: &Command) -> String {
        let name = name.into();
        generate_name(&name, cmd).into_owned()
    }
}
