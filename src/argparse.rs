//! A small command-line parser that reproduces the observable behavior of
//! Python's `argparse` (3.14) for the shapes this tool uses: store, append,
//! store-true, help, version, and one sub-command positional.
//!
//! Matching argparse exactly keeps every message, usage line, abbreviation
//! rule, and exit status identical to the 0.3.x releases.

use crate::pyfmt::{char_len, parse_float, parse_int, repr_str};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueType {
    Str,
    Int,
    Float,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ActionKind {
    Help,
    Version(String),
    StoreTrue,
    Store(ValueType),
    Append(ValueType),
    Subparsers,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    None,
    Bool(bool),
    Str(String),
    Int(i64),
    Float(f64),
    List(Vec<Value>),
}

#[derive(Debug, Clone)]
pub struct Action {
    pub option_strings: Vec<&'static str>,
    pub dest: &'static str,
    pub kind: ActionKind,
    pub choices: Option<Vec<&'static str>>,
    pub help: Option<&'static str>,
    pub default: Value,
}

impl Action {
    pub fn option(strings: &[&'static str], dest: &'static str, kind: ActionKind) -> Self {
        let default = match kind {
            ActionKind::StoreTrue => Value::Bool(false),
            ActionKind::Append(_) => Value::List(Vec::new()),
            _ => Value::None,
        };
        Self {
            option_strings: strings.to_vec(),
            dest,
            kind,
            choices: None,
            help: None,
            default,
        }
    }

    pub fn help(mut self, help: &'static str) -> Self {
        self.help = Some(help);
        self
    }

    pub fn choices(mut self, choices: &[&'static str]) -> Self {
        self.choices = Some(choices.to_vec());
        self
    }

    pub fn default(mut self, value: Value) -> Self {
        self.default = value;
        self
    }

    fn nargs_zero(&self) -> bool {
        matches!(
            self.kind,
            ActionKind::Help | ActionKind::Version(_) | ActionKind::StoreTrue
        )
    }

    fn is_positional(&self) -> bool {
        self.option_strings.is_empty()
    }

    fn name(&self) -> String {
        if self.is_positional() {
            self.dest.to_string()
        } else {
            self.option_strings.join("/")
        }
    }

    fn metavar(&self) -> String {
        match &self.choices {
            Some(choices) => format!("{{{}}}", choices.join(",")),
            None if self.is_positional() => self.dest.to_string(),
            None => self.dest.to_uppercase(),
        }
    }

    fn format_args(&self) -> String {
        match self.kind {
            ActionKind::Subparsers => format!("{} ...", self.metavar()),
            _ => self.metavar(),
        }
    }

    fn invocation(&self) -> String {
        if self.is_positional() {
            self.metavar()
        } else if self.nargs_zero() {
            self.option_strings.join(", ")
        } else {
            format!("{} {}", self.option_strings.join(", "), self.format_args())
        }
    }
}

/// A sub-command registered on a [`ActionKind::Subparsers`] action.
#[derive(Debug, Clone)]
pub struct SubCommand {
    pub name: &'static str,
    pub help: Option<&'static str>,
    pub parser: Parser,
}

#[derive(Debug, Clone)]
pub struct Parser {
    pub prog: String,
    pub description: Option<&'static str>,
    pub actions: Vec<Action>,
    pub subcommands: Vec<SubCommand>,
}

/// Parsed values keyed by destination.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Namespace {
    values: Vec<(&'static str, Value)>,
    unrecognized: Vec<String>,
}

impl Namespace {
    pub fn get(&self, dest: &str) -> &Value {
        self.values
            .iter()
            .find(|(key, _)| *key == dest)
            .map_or(&Value::None, |(_, value)| value)
    }

    fn set(&mut self, dest: &'static str, value: Value) {
        if let Some(entry) = self.values.iter_mut().find(|(key, _)| *key == dest) {
            entry.1 = value;
        } else {
            self.values.push((dest, value));
        }
    }

    pub fn str(&self, dest: &str) -> Option<&str> {
        match self.get(dest) {
            Value::Str(value) => Some(value),
            _ => None,
        }
    }

    pub fn flag(&self, dest: &str) -> bool {
        matches!(self.get(dest), Value::Bool(true))
    }

    pub fn int(&self, dest: &str) -> i64 {
        match self.get(dest) {
            Value::Int(value) => *value,
            _ => 0,
        }
    }

    pub fn float(&self, dest: &str) -> f64 {
        match self.get(dest) {
            Value::Float(value) => *value,
            Value::Int(value) => *value as f64,
            _ => 0.0,
        }
    }

    pub fn strings(&self, dest: &str) -> Vec<String> {
        match self.get(dest) {
            Value::List(values) => values
                .iter()
                .filter_map(|value| match value {
                    Value::Str(text) => Some(text.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// Terminal output requested by the parser: help, version, or a usage error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exit {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

struct ArgumentError {
    name: Option<String>,
    message: String,
}

impl ArgumentError {
    fn new(action: Option<&Action>, message: String) -> Self {
        Self {
            name: action.map(Action::name),
            message,
        }
    }

    fn render(&self) -> String {
        match &self.name {
            Some(name) => format!("argument {name}: {}", self.message),
            None => self.message.clone(),
        }
    }
}

enum Fail {
    Arg(ArgumentError),
    Exit(Exit),
}

impl From<ArgumentError> for Fail {
    fn from(error: ArgumentError) -> Self {
        Fail::Arg(error)
    }
}

#[derive(Debug, Clone)]
struct OptionTuple {
    action: Option<usize>,
    option_string: String,
    sep: Option<String>,
    explicit_arg: Option<String>,
}

/// argparse's `_negative_number_matcher` in Python 3.14: `-\.?\d` matched
/// at the start, so `-1e5`, `-1.` and `-2x` are positional values too.
fn is_negative_number(arg: &str) -> bool {
    let Some(rest) = arg.strip_prefix('-') else {
        return false;
    };
    let rest = rest.strip_prefix('.').unwrap_or(rest);
    rest.chars().next().is_some_and(crate::pyfmt::is_decimal)
}

impl Parser {
    pub fn new(prog: impl Into<String>, description: Option<&'static str>) -> Self {
        Self {
            prog: prog.into(),
            description,
            actions: vec![
                Action::option(&["-h", "--help"], "help", ActionKind::Help)
                    .help("show this help message and exit"),
            ],
            subcommands: Vec::new(),
        }
    }

    pub fn add(&mut self, action: Action) {
        self.actions.push(action);
    }

    fn option_strings(&self) -> impl Iterator<Item = (&'static str, usize)> + '_ {
        self.actions
            .iter()
            .enumerate()
            .flat_map(|(index, action)| action.option_strings.iter().map(move |s| (*s, index)))
    }

    fn lookup(&self, option: &str) -> Option<usize> {
        self.option_strings()
            .find(|(string, _)| *string == option)
            .map(|(_, index)| index)
    }

    fn get_option_tuples(&self, option: &str) -> Vec<OptionTuple> {
        let mut result = Vec::new();
        let mut chars = option.chars();
        let first = chars.next();
        let second = chars.next();
        let (prefix, sep, explicit) = match option.split_once('=') {
            Some((prefix, explicit)) => (prefix, Some("=".to_string()), Some(explicit.to_string())),
            None => (option, None, None),
        };
        if first == Some('-') && second == Some('-') {
            for (string, index) in self.option_strings() {
                if string.starts_with(prefix) {
                    result.push(OptionTuple {
                        action: Some(index),
                        option_string: string.to_string(),
                        sep: sep.clone(),
                        explicit_arg: explicit.clone(),
                    });
                }
            }
        } else if first == Some('-') {
            let split = option
                .char_indices()
                .nth(2)
                .map_or(option.len(), |(index, _)| index);
            let short_prefix = &option[..split];
            let short_explicit = &option[split..];
            for (string, index) in self.option_strings() {
                if string == short_prefix {
                    result.push(OptionTuple {
                        action: Some(index),
                        option_string: string.to_string(),
                        sep: Some(String::new()),
                        explicit_arg: Some(short_explicit.to_string()),
                    });
                } else if string.starts_with(prefix) {
                    result.push(OptionTuple {
                        action: Some(index),
                        option_string: string.to_string(),
                        sep: sep.clone(),
                        explicit_arg: explicit.clone(),
                    });
                }
            }
        }
        result
    }

    fn parse_optional(&self, arg: &str) -> Option<Vec<OptionTuple>> {
        if !arg.starts_with('-') {
            return None;
        }
        if let Some(index) = self.lookup(arg) {
            return Some(vec![OptionTuple {
                action: Some(index),
                option_string: arg.to_string(),
                sep: None,
                explicit_arg: None,
            }]);
        }
        if char_len(arg) == 1 {
            return None;
        }
        if let Some((option, explicit)) = arg.split_once('=')
            && let Some(index) = self.lookup(option)
        {
            return Some(vec![OptionTuple {
                action: Some(index),
                option_string: option.to_string(),
                sep: Some("=".to_string()),
                explicit_arg: Some(explicit.to_string()),
            }]);
        }
        let tuples = self.get_option_tuples(arg);
        if !tuples.is_empty() {
            return Some(tuples);
        }
        if is_negative_number(arg) || arg.contains(' ') {
            return None;
        }
        Some(vec![OptionTuple {
            action: None,
            option_string: arg.to_string(),
            sep: None,
            explicit_arg: None,
        }])
    }

    fn defaults(&self) -> Namespace {
        let mut namespace = Namespace::default();
        for action in &self.actions {
            if matches!(action.kind, ActionKind::Help | ActionKind::Version(_)) {
                continue;
            }
            namespace.set(action.dest, action.default.clone());
        }
        namespace
    }

    /// `parse_args`: unknown arguments are a usage error.
    pub fn parse_args(&self, args: &[String]) -> Result<Namespace, Exit> {
        let (namespace, extras) = self.parse_known_args(args)?;
        if !extras.is_empty() {
            return Err(self.error(&format!("unrecognized arguments: {}", extras.join(" "))));
        }
        Ok(namespace)
    }

    pub fn parse_known_args(&self, args: &[String]) -> Result<(Namespace, Vec<String>), Exit> {
        let mut namespace = self.defaults();
        match self.parse_inner(args, &mut namespace) {
            Ok(mut extras) => {
                extras.append(&mut namespace.unrecognized);
                Ok((namespace, extras))
            }
            Err(Fail::Arg(error)) => Err(self.error(&error.render())),
            Err(Fail::Exit(exit)) => Err(exit),
        }
    }

    /// Print usage plus `prog: error: message` to stderr and exit 2.
    pub fn error(&self, message: &str) -> Exit {
        Exit {
            code: 2,
            stdout: String::new(),
            stderr: format!("{}{}: error: {message}\n", self.format_usage(), self.prog),
        }
    }

    fn match_argument(&self, action: &Action, pattern: &[u8]) -> Result<usize, ArgumentError> {
        if action.nargs_zero() {
            return Ok(0);
        }
        if pattern.first() == Some(&b'A') {
            Ok(1)
        } else {
            Err(ArgumentError::new(
                Some(action),
                "expected one argument".to_string(),
            ))
        }
    }

    fn convert(&self, action: &Action, value: &str) -> Result<Value, ArgumentError> {
        let kind = match action.kind {
            ActionKind::Store(kind) | ActionKind::Append(kind) => kind,
            _ => ValueType::Str,
        };
        let converted = match kind {
            ValueType::Str => Some(Value::Str(value.to_string())),
            ValueType::Int => parse_int(value).map(Value::Int),
            ValueType::Float => parse_float(value).map(Value::Float),
        };
        converted.ok_or_else(|| {
            let name = match kind {
                ValueType::Int => "int",
                ValueType::Float => "float",
                ValueType::Str => "str",
            };
            ArgumentError::new(
                Some(action),
                format!("invalid {name} value: {}", repr_str(value)),
            )
        })
    }

    fn check_choice(&self, action: &Action, value: &str) -> Result<(), ArgumentError> {
        let Some(choices) = &action.choices else {
            return Ok(());
        };
        if choices.contains(&value) {
            return Ok(());
        }
        let rendered: Vec<String> = choices.iter().map(|choice| repr_str(choice)).collect();
        Err(ArgumentError::new(
            Some(action),
            format!(
                "invalid choice: {} (choose from {})",
                repr_str(value),
                rendered.join(", ")
            ),
        ))
    }

    fn take_action(
        &self,
        index: usize,
        args: Vec<String>,
        namespace: &mut Namespace,
    ) -> Result<(), Fail> {
        let action = &self.actions[index];
        match &action.kind {
            ActionKind::Help => Err(Fail::Exit(Exit {
                code: 0,
                stdout: self.format_help(),
                stderr: String::new(),
            })),
            ActionKind::Version(version) => Err(Fail::Exit(Exit {
                code: 0,
                stdout: format_version(version),
                stderr: String::new(),
            })),
            ActionKind::StoreTrue => {
                namespace.set(action.dest, Value::Bool(true));
                Ok(())
            }
            ActionKind::Store(_) | ActionKind::Append(_) => {
                let raw = args.first().cloned().unwrap_or_default();
                let value = self.convert(action, &raw)?;
                self.check_choice(action, &raw)?;
                if matches!(action.kind, ActionKind::Append(_)) {
                    let mut list = match namespace.get(action.dest) {
                        Value::List(items) => items.clone(),
                        _ => Vec::new(),
                    };
                    list.push(value);
                    namespace.set(action.dest, Value::List(list));
                } else {
                    namespace.set(action.dest, value);
                }
                Ok(())
            }
            ActionKind::Subparsers => {
                let name = args.first().cloned().unwrap_or_default();
                self.check_choice(action, &name)?;
                namespace.set(action.dest, Value::Str(name.clone()));
                let Some(sub) = self.subcommands.iter().find(|sub| sub.name == name) else {
                    return Ok(());
                };
                let (sub_namespace, extras) = sub
                    .parser
                    .parse_known_args(&args[1..])
                    .map_err(Fail::Exit)?;
                for (key, value) in sub_namespace.values {
                    namespace.set(key, value);
                }
                namespace.unrecognized.extend(extras);
                Ok(())
            }
        }
    }

    fn parse_inner(&self, args: &[String], namespace: &mut Namespace) -> Result<Vec<String>, Fail> {
        let mut option_indices: Vec<Option<Vec<OptionTuple>>> = vec![None; args.len()];
        let mut pattern: Vec<u8> = Vec::with_capacity(args.len());
        for (index, arg) in args.iter().enumerate() {
            if arg == "--" {
                pattern.push(b'-');
                pattern.extend(std::iter::repeat_n(b'A', args.len() - index - 1));
                break;
            }
            match self.parse_optional(arg) {
                None => pattern.push(b'A'),
                Some(tuples) => {
                    option_indices[index] = Some(tuples);
                    pattern.push(b'O');
                }
            }
        }

        let mut positionals: Vec<usize> = self
            .actions
            .iter()
            .enumerate()
            .filter(|(_, action)| action.is_positional())
            .map(|(index, _)| index)
            .collect();
        let mut extras: Vec<String> = Vec::new();
        let max_option_index = option_indices.iter().rposition(Option::is_some);

        let consume_positionals = |start: usize,
                                   positionals: &mut Vec<usize>,
                                   namespace: &mut Namespace|
         -> Result<usize, Fail> {
            let Some(&first) = positionals.first() else {
                return Ok(start);
            };
            // Only `nargs=PARSER` positionals exist: pattern `-*A[-AO]*`.
            let rest = &pattern[start..];
            let dashes = rest.iter().take_while(|c| **c == b'-').count();
            if rest.get(dashes) != Some(&b'A') {
                return Ok(start);
            }
            let count = rest.len();
            let mut values: Vec<String> = args[start..start + count].to_vec();
            if pattern[start] == b'-' {
                values.remove(0);
            }
            positionals.remove(0);
            self.take_action(first, values, namespace)?;
            Ok(start + count)
        };

        let mut start_index = 0usize;
        if let Some(max_option_index) = max_option_index {
            while start_index <= max_option_index {
                let mut next_option = start_index;
                while next_option <= max_option_index && option_indices[next_option].is_none() {
                    next_option += 1;
                }
                if start_index != next_option {
                    let end = consume_positionals(start_index, &mut positionals, namespace)?;
                    if end > start_index {
                        start_index = end;
                        continue;
                    }
                    start_index = end;
                }
                if option_indices.get(start_index).is_none_or(Option::is_none) {
                    extras.extend(args[start_index..next_option].iter().cloned());
                    start_index = next_option;
                }
                start_index = self.consume_optional(
                    start_index,
                    args,
                    &pattern,
                    &option_indices,
                    &mut extras,
                    namespace,
                )?;
            }
        }
        let stop = consume_positionals(start_index, &mut positionals, namespace)?;
        extras.extend(args[stop..].iter().cloned());
        Ok(extras)
    }

    fn consume_optional(
        &self,
        start_index: usize,
        args: &[String],
        pattern: &[u8],
        option_indices: &[Option<Vec<OptionTuple>>],
        extras: &mut Vec<String>,
        namespace: &mut Namespace,
    ) -> Result<usize, Fail> {
        let tuples = option_indices[start_index]
            .as_ref()
            .expect("option index recorded");
        if tuples.len() > 1 {
            let options: Vec<&str> = tuples.iter().map(|t| t.option_string.as_str()).collect();
            return Err(ArgumentError::new(
                None,
                format!(
                    "ambiguous option: {} could match {}",
                    args[start_index],
                    options.join(", ")
                ),
            )
            .into());
        }
        let tuple = tuples[0].clone();
        let Some(mut action_index) = tuple.action else {
            extras.push(args[start_index].clone());
            return Ok(start_index + 1);
        };
        let mut option_string = tuple.option_string;
        let mut sep = tuple.sep;
        let mut explicit_arg = tuple.explicit_arg;
        let mut action_tuples: Vec<(usize, Vec<String>)> = Vec::new();
        let stop;
        loop {
            let action = &self.actions[action_index];
            if let Some(explicit) = explicit_arg.clone() {
                let arg_count = self.match_argument(action, b"A")?;
                let second = option_string.chars().nth(1);
                if arg_count == 0 && second != Some('-') && !explicit.is_empty() {
                    let sep_truthy = sep.as_deref().is_some_and(|s| !s.is_empty());
                    if sep_truthy || explicit.starts_with('-') {
                        return Err(ArgumentError::new(
                            Some(action),
                            format!("ignored explicit argument {}", repr_str(&explicit)),
                        )
                        .into());
                    }
                    action_tuples.push((action_index, Vec::new()));
                    let prefix_char = option_string.chars().next().unwrap_or('-');
                    let mut explicit_chars = explicit.chars();
                    let next_char = explicit_chars.next().unwrap_or_default();
                    option_string = format!("{prefix_char}{next_char}");
                    match self.lookup(&option_string) {
                        Some(next_index) => {
                            action_index = next_index;
                            let remaining = explicit_chars.as_str().to_string();
                            if remaining.is_empty() {
                                sep = None;
                                explicit_arg = None;
                            } else if let Some(stripped) = remaining.strip_prefix('=') {
                                sep = Some("=".to_string());
                                explicit_arg = Some(stripped.to_string());
                            } else {
                                sep = Some(String::new());
                                explicit_arg = Some(remaining);
                            }
                        }
                        None => {
                            extras.push(format!("{prefix_char}{explicit}"));
                            stop = start_index + 1;
                            break;
                        }
                    }
                } else if arg_count == 1 {
                    stop = start_index + 1;
                    action_tuples.push((action_index, vec![explicit]));
                    break;
                } else {
                    return Err(ArgumentError::new(
                        Some(action),
                        format!("ignored explicit argument {}", repr_str(&explicit)),
                    )
                    .into());
                }
            } else {
                let start = start_index + 1;
                let arg_count = self.match_argument(action, &pattern[start..])?;
                stop = start + arg_count;
                action_tuples.push((action_index, args[start..stop].to_vec()));
                break;
            }
        }
        for (index, values) in action_tuples {
            self.take_action(index, values, namespace)?;
        }
        Ok(stop)
    }

    // -----------------------------------------------------------------
    // Help formatting (argparse.HelpFormatter)
    // -----------------------------------------------------------------

    fn usage_parts(&self) -> (Vec<String>, usize) {
        let mut parts = Vec::new();
        for action in self.actions.iter().filter(|a| !a.is_positional()) {
            if action.nargs_zero() {
                parts.push(format!("[{}]", action.option_strings[0]));
            } else {
                parts.push(format!(
                    "[{} {}]",
                    action.option_strings[0],
                    action.format_args()
                ));
            }
        }
        let pos_start = parts.len();
        for action in self.actions.iter().filter(|a| a.is_positional()) {
            parts.push(action.format_args());
        }
        (parts, pos_start)
    }

    fn usage_text(&self, width: isize) -> String {
        let prefix = "usage: ";
        let prog = self.prog.as_str();
        let (parts, pos_start) = self.usage_parts();
        let mut usage = std::iter::once(prog.to_string())
            .chain(parts.iter().filter(|p| !p.is_empty()).cloned())
            .collect::<Vec<_>>()
            .join(" ");
        let text_width = width;
        let prefix_len = char_len(prefix) as isize;
        if prefix_len + char_len(&usage) as isize > text_width {
            let opt_parts = &parts[..pos_start];
            let pos_parts = &parts[pos_start..];
            let get_lines = |parts: &[String], indent: &str, prefix: Option<&str>| -> Vec<String> {
                let mut lines: Vec<String> = Vec::new();
                let mut line: Vec<&str> = Vec::new();
                let indent_length = char_len(indent) as isize;
                let mut line_len = match prefix {
                    Some(prefix) => char_len(prefix) as isize - 1,
                    None => indent_length - 1,
                };
                for part in parts {
                    let part_len = char_len(part) as isize;
                    if line_len + 1 + part_len > text_width && !line.is_empty() {
                        lines.push(format!("{indent}{}", line.join(" ")));
                        line.clear();
                        line_len = indent_length - 1;
                    }
                    line.push(part);
                    line_len += part_len + 1;
                }
                if !line.is_empty() {
                    lines.push(format!("{indent}{}", line.join(" ")));
                }
                if prefix.is_some()
                    && let Some(first) = lines.first_mut()
                {
                    *first = first.chars().skip(indent_length as usize).collect();
                }
                lines
            };
            let prog_len = char_len(prog) as isize;
            let lines: Vec<String> = if (prefix_len + prog_len) as f64 <= 0.75 * text_width as f64 {
                let indent = " ".repeat((prefix_len + prog_len + 1) as usize);
                if !opt_parts.is_empty() {
                    let mut head = vec![prog.to_string()];
                    head.extend(opt_parts.iter().cloned());
                    let mut lines = get_lines(&head, &indent, Some(prefix));
                    lines.extend(get_lines(pos_parts, &indent, None));
                    lines
                } else if !pos_parts.is_empty() {
                    let mut head = vec![prog.to_string()];
                    head.extend(pos_parts.iter().cloned());
                    get_lines(&head, &indent, Some(prefix))
                } else {
                    vec![prog.to_string()]
                }
            } else {
                let indent = " ".repeat(prefix_len as usize);
                let all: Vec<String> = parts.to_vec();
                let mut lines = get_lines(&all, &indent, None);
                if lines.len() > 1 {
                    lines = get_lines(opt_parts, &indent, None);
                    lines.extend(get_lines(pos_parts, &indent, None));
                }
                let mut with_prog = vec![prog.to_string()];
                with_prog.extend(lines);
                with_prog
            };
            usage = lines.join("\n");
        }
        format!("{prefix}{usage}\n\n")
    }

    pub fn format_usage(&self) -> String {
        finish_help(&self.usage_text(help_width()))
    }

    fn subactions(&self, action: &Action) -> Vec<(String, Option<&'static str>)> {
        if !matches!(action.kind, ActionKind::Subparsers) {
            return Vec::new();
        }
        self.subcommands
            .iter()
            .filter(|sub| sub.help.is_some())
            .map(|sub| (sub.name.to_string(), sub.help))
            .collect()
    }

    pub fn format_help(&self) -> String {
        let width = help_width();
        let max_help_position = 24.min((width - 20).max(4));
        let mut action_max_length = 0isize;
        for action in &self.actions {
            action_max_length = action_max_length.max(char_len(&action.invocation()) as isize + 2);
            for (name, _) in self.subactions(action) {
                action_max_length = action_max_length.max(char_len(&name) as isize + 4);
            }
        }
        let layout = Layout {
            width,
            help_position: (action_max_length + 2).min(max_help_position),
        };

        let mut out = self.usage_text(width);
        if let Some(description) = self.description {
            out.push_str(&fill(description, width.max(11), ""));
            out.push_str("\n\n");
        }
        let sections: [(&str, Vec<&Action>); 2] = [
            (
                "positional arguments",
                self.actions.iter().filter(|a| a.is_positional()).collect(),
            ),
            (
                "options",
                self.actions.iter().filter(|a| !a.is_positional()).collect(),
            ),
        ];
        for (heading, actions) in sections {
            let mut items = String::new();
            for action in actions {
                items.push_str(&layout.format_action(
                    &action.invocation(),
                    action.help,
                    2,
                    &self.subactions(action),
                ));
            }
            if !items.is_empty() {
                out.push('\n');
                out.push_str(heading);
                out.push_str(":\n");
                out.push_str(&items);
                out.push('\n');
            }
        }
        finish_help(&out)
    }
}

struct Layout {
    width: isize,
    help_position: isize,
}

impl Layout {
    fn format_action(
        &self,
        header: &str,
        help: Option<&str>,
        indent: isize,
        subactions: &[(String, Option<&'static str>)],
    ) -> String {
        let help_width = (self.width - self.help_position).max(11);
        let action_width = self.help_position - indent - 2;
        let pad = |count: isize| " ".repeat(count.max(0) as usize);
        let help = help.filter(|text| !text.is_empty());
        let mut parts = String::new();
        let mut indent_first = 0;
        let header_text = match help {
            None => format!("{}{header}\n", pad(indent)),
            Some(_) if char_len(header) as isize <= action_width => {
                let padding = action_width - char_len(header) as isize;
                format!("{}{header}{}  ", pad(indent), pad(padding))
            }
            Some(_) => {
                indent_first = self.help_position;
                format!("{}{header}\n", pad(indent))
            }
        };
        parts.push_str(&header_text);
        match help.filter(|text| !crate::pyfmt::strip(text).is_empty()) {
            Some(text) => {
                let lines = split_lines(text, help_width);
                if let Some((first, rest)) = lines.split_first() {
                    parts.push_str(&format!("{}{first}\n", pad(indent_first)));
                    for line in rest {
                        parts.push_str(&format!("{}{line}\n", pad(self.help_position)));
                    }
                }
            }
            None => {
                if !header_text.ends_with('\n') {
                    parts.push('\n');
                }
            }
        }
        for (name, sub_help) in subactions {
            parts.push_str(&self.format_action(name, *sub_help, indent + 2, &[]));
        }
        parts
    }
}

/// `format_help()` post-processing: collapse 3+ newlines, strip, add `\n`.
fn finish_help(text: &str) -> String {
    let mut collapsed = String::with_capacity(text.len());
    let mut newlines = 0;
    for c in text.chars() {
        if c == '\n' {
            newlines += 1;
            continue;
        }
        if newlines > 0 {
            collapsed.push_str(if newlines >= 3 {
                "\n\n"
            } else {
                &"\n\n"[..newlines]
            });
            newlines = 0;
        }
        collapsed.push(c);
    }
    let trimmed = collapsed.trim_matches('\n');
    if trimmed.is_empty() {
        return String::new();
    }
    format!("{trimmed}\n")
}

fn format_version(version: &str) -> String {
    let width = help_width();
    finish_help(&format!("{}\n\n", fill(version, width.max(11), "")))
}

/// Formatter width: `shutil.get_terminal_size().columns - 2`.
pub fn help_width() -> isize {
    terminal_columns() as isize - 2
}

fn terminal_columns() -> i64 {
    if let Some(columns) = std::env::var("COLUMNS")
        .ok()
        .and_then(|value| parse_int(&value))
        .filter(|columns| *columns > 0)
    {
        return columns.min(i64::from(u32::MAX));
    }
    match rustix::termios::tcgetwinsize(std::io::stdout()) {
        Ok(size) if size.ws_col > 0 => i64::from(size.ws_col),
        _ => 80,
    }
}

fn collapse_whitespace(text: &str) -> String {
    text.split(|c: char| c.is_ascii_whitespace() || c == '\u{0b}')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_letter(c: Option<&char>) -> bool {
    c.is_some_and(|c| c.is_alphabetic() || *c == '_')
}

/// textwrap's chunking: whitespace runs, and words split after hyphens that
/// join two letter runs (`wordsep_re`).
fn chunks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let start = index;
        if chars[index] == ' ' {
            while index < chars.len() && chars[index] == ' ' {
                index += 1;
            }
            out.push(chars[start..index].iter().collect());
            continue;
        }
        while index < chars.len() && chars[index] != ' ' {
            let c = chars[index];
            index += 1;
            if c == '-' && index - 1 > start {
                let at = index - 1;
                let behind_two =
                    at >= 2 && is_letter(chars.get(at - 1)) && is_letter(chars.get(at - 2));
                let behind_alt = at >= 3
                    && is_letter(chars.get(at - 1))
                    && chars.get(at - 2) == Some(&'-')
                    && is_letter(chars.get(at - 3));
                let ahead = is_letter(chars.get(at + 1))
                    && (is_letter(chars.get(at + 2))
                        || (chars.get(at + 2) == Some(&'-') && is_letter(chars.get(at + 3))));
                if (behind_two || behind_alt) && ahead {
                    break;
                }
            }
        }
        out.push(chars[start..index].iter().collect());
    }
    out
}

/// `textwrap.wrap(text, width)` after argparse's whitespace collapsing.
fn wrap(text: &str, width: isize, indent: &str) -> Vec<String> {
    let text = collapse_whitespace(text);
    let mut pending: Vec<String> = chunks(&text);
    pending.reverse();
    let mut lines: Vec<String> = Vec::new();
    while !pending.is_empty() {
        let mut line: Vec<String> = Vec::new();
        let mut line_len = 0isize;
        let line_width = width - char_len(indent) as isize;
        if !lines.is_empty() && pending.last().is_some_and(|c| c.trim().is_empty()) {
            pending.pop();
        }
        while let Some(chunk) = pending.last() {
            let length = char_len(chunk) as isize;
            if line_len + length <= line_width {
                line_len += length;
                line.push(pending.pop().unwrap_or_default());
            } else {
                break;
            }
        }
        if let Some(chunk) = pending.last()
            && char_len(chunk) as isize > line_width
        {
            let space_left = if line_width < 1 {
                1
            } else {
                line_width - line_len
            };
            let chunk_chars: Vec<char> = chunk.chars().collect();
            let mut end = space_left.max(0) as usize;
            if (chunk_chars.len() as isize) > space_left {
                let limit = (space_left.max(0) as usize).min(chunk_chars.len());
                if let Some(hyphen) = chunk_chars[..limit].iter().rposition(|c| *c == '-')
                    && hyphen > 0
                    && chunk_chars[..hyphen].iter().any(|c| *c != '-')
                {
                    end = hyphen + 1;
                }
            }
            let end = end.min(chunk_chars.len());
            line.push(chunk_chars[..end].iter().collect());
            let remaining: String = chunk_chars[end..].iter().collect();
            if let Some(last) = pending.last_mut() {
                *last = remaining;
            }
            if pending.last().is_some_and(String::is_empty) {
                pending.pop();
            }
        }
        if line.last().is_some_and(|c| c.trim().is_empty()) {
            line.pop();
        }
        if !line.is_empty() {
            lines.push(format!("{indent}{}", line.concat()));
        }
    }
    lines
}

fn split_lines(text: &str, width: isize) -> Vec<String> {
    wrap(text, width, "")
}

fn fill(text: &str, width: isize, indent: &str) -> String {
    wrap(text, width, indent).join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_split_letter_hyphens_only() {
        assert_eq!(
            chunks("not every systemctl-discovered unit"),
            [
                "not",
                " ",
                "every",
                " ",
                "systemctl-",
                "discovered",
                " ",
                "unit"
            ]
        );
        assert_eq!(
            chunks("keep/retire a-b 1-2"),
            ["keep/retire", " ", "a-b", " ", "1-2"]
        );
    }

    #[test]
    fn wrap_matches_textwrap() {
        assert_eq!(
            wrap(
                "only report unit files found on disk, not every systemctl-discovered unit",
                54,
                ""
            ),
            [
                "only report unit files found on disk, not every",
                "systemctl-discovered unit"
            ]
        );
        assert_eq!(
            wrap(
                "only report unit files found on disk, not every systemctl-discovered unit",
                30,
                ""
            ),
            [
                "only report unit files found",
                "on disk, not every systemctl-",
                "discovered unit"
            ]
        );
    }

    #[test]
    fn negative_numbers_are_values() {
        assert!(is_negative_number("-5"));
        assert!(is_negative_number("-.5"));
        assert!(is_negative_number("-1.5"));
        assert!(!is_negative_number("-x"));
        assert!(!is_negative_number("-"));
        assert!(!is_negative_number("--5"));
        assert!(!is_negative_number("-."));
        // Python 3.14 only checks the prefix `-\.?\d` (Unicode digits).
        assert!(is_negative_number("-1."));
        assert!(is_negative_number("-1e5"));
        assert!(is_negative_number("-2x"));
        assert!(is_negative_number("-\u{661}"));
    }
}
