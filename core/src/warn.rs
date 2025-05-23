use crate::{__ToConsoleString as ToConsoleString, LightContext};
use ansi_term::{
    Color::{Green, Yellow},
    Style,
};
use anyhow::{Result, bail};
use bitflags::bitflags;
use heck::ToKebabCase;
use std::{collections::BTreeMap, io::IsTerminal, sync::Mutex};

// smoelius: `Warning` is part of Necessist's public API. Please try to follow the naming convention
// of `what` (e.g., `Output`) followed by `why` (e.g., `Invalid`).
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
#[non_exhaustive]
#[remain::sorted]
pub enum Warning {
    All,
    DatabaseDoesNotExist,
    DryRunFailed,
    FilesChanged,
    IgnoredFunctionsUnsupported,
    IgnoredMacrosUnsupported,
    IgnoredMethodsUnsupported,
    InstrumentationNonbuildable,
    ItMessageNotFound,
    LocalFunctionAmbiguous,
    ModulePathUnknown,
    OptionDeprecated,
    OutputInvalid,
    ParsingFailed,
    RunTestFailed,
}

impl std::fmt::Display for Warning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", format!("{self:?}").to_kebab_case())
    }
}

bitflags! {
    #[derive(Clone, Copy)]
    pub struct Flags: u8 {
        const ONCE = 1 << 0;
    }
}

/// Like [`warn`], but prints the warning prefixed with its source.
#[allow(clippy::module_name_repetitions)]
pub fn source_warn(
    context: &LightContext,
    warning: Warning,
    source: &dyn ToConsoleString,
    msg: &str,
    flags: Flags,
) -> Result<()> {
    warn_internal(context, warning, Some(source), msg, flags)
}

/// Prints a warning message to the console.
///
/// # Arguments
///
/// * `context` - The context to use for printing the warning.
/// * `warning` - The type of the warning.
/// * `msg` - The message to print.
/// * `flags` - The flags to use for printing the warning.
///
/// # Errors
///
/// Returns an error if the message could not be printed.
pub fn warn(context: &LightContext, warning: Warning, msg: &str, flags: Flags) -> Result<()> {
    warn_internal(context, warning, None, msg, flags)
}

const BUG_MSG: &str = "

This may indicate a bug in Necessist. Consider opening an issue at: \
https://github.com/trailofbits/necessist/issues
";

bitflags! {
    struct State: u8 {
        const ALLOW_MSG_EMITTED = 1 << 0;
        const BUG_MSG_EMITTED = 1 << 1;
        const WARNING_EMITTED = 1 << 2;
    }
}

static WARNING_STATE_MAP: Mutex<BTreeMap<Warning, State>> = Mutex::new(BTreeMap::new());

#[cfg_attr(dylint_lib = "general", allow(non_local_effect_before_error_return))]
fn warn_internal(
    context: &LightContext,
    warning: Warning,
    source: Option<&dyn ToConsoleString>,
    msg: &str,
    flags: Flags,
) -> Result<()> {
    assert_ne!(warning, Warning::All);

    #[allow(clippy::unwrap_used)]
    let mut warning_state_map = WARNING_STATE_MAP.lock().unwrap();

    let state = warning_state_map
        .entry(warning)
        .or_insert_with(State::empty);

    // smoelius: Append `BUG_MSG` to `msg` in case we have to `bail!`.
    let msg = msg.to_owned()
        + if may_be_bug(warning) && !state.contains(State::BUG_MSG_EMITTED) {
            state.insert(State::BUG_MSG_EMITTED);
            BUG_MSG
        } else {
            ""
        };

    if context.opts.deny.contains(&Warning::All) || context.opts.deny.contains(&warning) {
        bail!(msg);
    }

    if context.opts.quiet
        || context.opts.allow.contains(&Warning::All)
        || context.opts.allow.contains(&warning)
        || (flags.contains(Flags::ONCE) && state.contains(State::WARNING_EMITTED))
    {
        return Ok(());
    }

    let allow_msg = if state.contains(State::ALLOW_MSG_EMITTED) {
        String::new()
    } else {
        state.insert(State::ALLOW_MSG_EMITTED);
        format!(
            "
Silence this warning with: --allow {warning}"
        )
    };

    (context.println)(&format!(
        "{}{}: {}{}",
        source.map_or(String::new(), |source| format!(
            "{}: ",
            source.to_console_string()
        )),
        if std::io::stdout().is_terminal() {
            Yellow.bold()
        } else {
            Style::default()
        }
        .paint("Warning"),
        msg,
        allow_msg
    ));

    state.insert(State::WARNING_EMITTED);

    Ok(())
}

pub(crate) fn note(context: &LightContext, msg: &str) {
    if context.opts.quiet {
        return;
    }

    (context.println)(&format!(
        "{}: {}",
        if std::io::stdout().is_terminal() {
            Green.bold()
        } else {
            Style::default()
        }
        .paint("Note"),
        msg
    ));
}

fn may_be_bug(warning: Warning) -> bool {
    match warning {
        Warning::All => unreachable!(),
        Warning::DatabaseDoesNotExist
        | Warning::DryRunFailed
        | Warning::FilesChanged
        | Warning::IgnoredFunctionsUnsupported
        | Warning::IgnoredMacrosUnsupported
        | Warning::IgnoredMethodsUnsupported
        | Warning::ItMessageNotFound
        | Warning::LocalFunctionAmbiguous
        | Warning::OptionDeprecated
        | Warning::OutputInvalid
        | Warning::ParsingFailed => false,
        Warning::InstrumentationNonbuildable
        | Warning::ModulePathUnknown
        | Warning::RunTestFailed => true,
    }
}
