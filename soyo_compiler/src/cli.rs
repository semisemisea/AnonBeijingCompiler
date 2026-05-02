use std::{
    ffi::OsString,
    fmt,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Config {
    pub(crate) input_path: PathBuf,
    pub(crate) output_path: PathBuf,
    pub(crate) optimize: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CliErrors {
    program: String,
    messages: Vec<String>,
}

impl CliErrors {
    fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            messages: Vec::new(),
        }
    }

    fn push(&mut self, message: impl Into<String>) {
        self.messages.push(message.into());
    }

    fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    fn render(&self, colored: bool) -> String {
        let mut output = String::new();
        for (i, message) in self.messages.iter().enumerate() {
            if i > 0 {
                output.push('\n');
            }
            if colored {
                output.push_str(&format!(
                    "{}: \x1b[0;1;31merror: \x1b[0m\x1b[1m{}\x1b[0m",
                    self.program, message
                ));
            } else {
                output.push_str(&format!("{}: error: {}", self.program, message));
            }
        }
        output
    }

    pub(crate) fn write_to_stderr(&self) -> io::Result<()> {
        let colored = io::stderr().is_terminal();
        let mut stderr = io::stderr().lock();
        writeln!(stderr, "{}", self.render(colored))
    }
}

impl fmt::Display for CliErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render(false))
    }
}

fn program_name(program: OsString) -> String {
    Path::new(&program)
        .file_name()
        .unwrap_or(&program)
        .to_string_lossy()
        .into_owned()
}

pub(crate) fn parse_env_config() -> Result<Config, CliErrors> {
    let mut args = std::env::args();
    let program = args
        .next()
        .map(OsString::from)
        .map(program_name)
        .unwrap_or_else(|| "compiler".to_string());
    parse_config(program, args)
}

pub(crate) fn parse_config<I, S>(program: impl Into<String>, args: I) -> Result<Config, CliErrors>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut errors = CliErrors::new(program);
    let mut input_paths = Vec::new();
    let mut output_path = None;
    let mut optimize = false;
    let mut emit_asm = false;
    let mut saw_output_option = false;
    let mut args = args
        .into_iter()
        .map(Into::into)
        .map(|arg| arg.to_string_lossy().into_owned());

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-S" => emit_asm = true,
            "-o" => {
                saw_output_option = true;
                match args.next() {
                    Some(value) => output_path = Some(PathBuf::from(value)),
                    None => errors.push("argument to '-o' is missing (expected 1 value)"),
                }
            }
            "-O1" => optimize = true,
            _ if arg.starts_with('-') => errors.push(format!("unknown argument: '{}'", arg)),
            _ => input_paths.push(PathBuf::from(arg)),
        }
    }

    if !emit_asm {
        errors.push("missing required argument '-S'");
    }

    if !saw_output_option {
        errors.push("missing required argument '-o'");
    }

    if input_paths.is_empty() {
        errors.push("no input files");
    }

    if input_paths.len() > 1 {
        if output_path.is_some() {
            errors.push("cannot specify -o when generating multiple output files");
        } else {
            errors.push("cannot specify multiple input files");
        }
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    let input_path = input_paths.remove(0);
    let output_path = output_path.unwrap();

    Ok(Config {
        input_path,
        output_path,
        optimize,
    })
}

#[cfg(test)]
mod tests {
    use super::{CliErrors, Config, parse_config};
    use std::path::PathBuf;

    fn parse(args: &[&str]) -> Result<Config, CliErrors> {
        parse_config("compiler", args.iter().copied())
    }

    #[test]
    fn parse_config_accepts_reordered_arguments() {
        assert_eq!(
            parse(&["-O1", "testcase.sy", "-S", "-o", "testcase.s"]).unwrap(),
            Config {
                input_path: PathBuf::from("testcase.sy"),
                output_path: PathBuf::from("testcase.s"),
                optimize: true,
            }
        );
    }

    #[test]
    fn parse_config_accepts_required_arguments_without_optimization() {
        assert_eq!(
            parse(&["testcase.sy", "-o", "testcase.s", "-S"]).unwrap(),
            Config {
                input_path: PathBuf::from("testcase.sy"),
                output_path: PathBuf::from("testcase.s"),
                optimize: false,
            }
        );
    }

    #[test]
    fn parse_config_rejects_joined_output_option() {
        assert_eq!(
            parse(&["-S", "-otestcase.s", "testcase.sy"])
                .unwrap_err()
                .to_string(),
            "compiler: error: unknown argument: '-otestcase.s'\ncompiler: error: missing required argument '-o'"
        );
    }

    #[test]
    fn parse_config_reports_missing_o_value_like_clang() {
        assert_eq!(
            parse(&["-S", "-o"]).unwrap_err().to_string(),
            "compiler: error: argument to '-o' is missing (expected 1 value)\ncompiler: error: no input files"
        );
    }

    #[test]
    fn parse_config_reports_unknown_argument_like_clang() {
        assert_eq!(
            parse(&["-foo"]).unwrap_err().to_string(),
            "compiler: error: unknown argument: '-foo'\ncompiler: error: missing required argument '-S'\ncompiler: error: missing required argument '-o'\ncompiler: error: no input files"
        );
    }

    #[test]
    fn parse_config_reports_unsupported_optimization_argument() {
        assert_eq!(
            parse(&["-S", "-O2", "-o", "testcase.s", "testcase.sy"])
                .unwrap_err()
                .to_string(),
            "compiler: error: unknown argument: '-O2'"
        );
    }

    #[test]
    fn parse_config_reports_multiple_outputs_like_clang() {
        assert_eq!(
            parse(&["-S", "-o", "testcase.s", "a.sy", "b.sy"])
                .unwrap_err()
                .to_string(),
            "compiler: error: cannot specify -o when generating multiple output files"
        );
    }

    #[test]
    fn cli_errors_render_colored_like_clang() {
        assert_eq!(
            parse(&["-foo"]).unwrap_err().render(true),
            "compiler: \x1b[0;1;31merror: \x1b[0m\x1b[1munknown argument: '-foo'\x1b[0m\ncompiler: \x1b[0;1;31merror: \x1b[0m\x1b[1mmissing required argument '-S'\x1b[0m\ncompiler: \x1b[0;1;31merror: \x1b[0m\x1b[1mmissing required argument '-o'\x1b[0m\ncompiler: \x1b[0;1;31merror: \x1b[0m\x1b[1mno input files\x1b[0m"
        );
    }
}
