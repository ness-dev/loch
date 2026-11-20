//! The subset of Python's argparse that loch's commands use, ported from
//! CPython 3.12.10 closely enough that usage errors and `-h` come out byte for
//! byte the same: abbreviated long options, `--opt=value`, `--`, the
//! A/O/- pattern matching that decides which strings a `nargs="?"` positional
//! takes, and the help formatter's wrapping. The harness diffs both builds
//! over a spread of bad command lines to keep it that way.

use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Nargs {
    /// Exactly one value; a positional so declared is required.
    One,
    /// `nargs="?"`.
    Optional,
    /// A flag: store_true or help.
    Zero,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Store,
    StoreTrue,
    Help,
}

#[derive(Clone)]
struct Action {
    options: Vec<&'static str>,
    dest: String,
    nargs: Nargs,
    kind: Kind,
    metavar: Option<&'static str>,
    constant: Option<&'static str>,
    default: Option<&'static str>,
    help: Option<&'static str>,
}

impl Action {
    fn required(&self) -> bool {
        self.options.is_empty() && self.nargs == Nargs::One
    }

    fn name(&self) -> String {
        if !self.options.is_empty() {
            self.options.join("/")
        } else if let Some(metavar) = self.metavar {
            metavar.into()
        } else {
            self.dest.clone()
        }
    }

    fn metavar(&self) -> String {
        match self.metavar {
            Some(metavar) => metavar.into(),
            None if self.options.is_empty() => self.dest.clone(),
            None => self.dest.to_uppercase(),
        }
    }

    fn format_args(&self) -> String {
        match self.nargs {
            Nargs::One => self.metavar(),
            Nargs::Optional => format!("[{}]", self.metavar()),
            Nargs::Zero => String::new(),
        }
    }

    fn invocation(&self) -> String {
        if self.options.is_empty() {
            return self.metavar();
        }
        if self.nargs == Nargs::Zero {
            return self.options.join(", ");
        }
        let args = self.format_args();
        self.options
            .iter()
            .map(|option| format!("{} {}", option, args))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn usage(&self) -> String {
        if self.options.is_empty() {
            return self.format_args();
        }
        let part = if self.nargs == Nargs::Zero {
            self.options[0].to_string()
        } else {
            format!("{} {}", self.options[0], self.format_args())
        };
        format!("[{}]", part)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    Str(Option<String>),
    Flag(bool),
}

pub struct Args {
    values: HashMap<String, Val>,
}

impl Args {
    pub fn str(&self, dest: &str) -> Option<String> {
        match self.values.get(dest) {
            Some(Val::Str(value)) => value.clone(),
            _ => None,
        }
    }

    pub fn flag(&self, dest: &str) -> bool {
        matches!(self.values.get(dest), Some(Val::Flag(true)))
    }
}

pub enum Exit {
    /// `-h`: the help text, which main renders as the command's result.
    Help(String),
    /// An argparse error: the message, and the prose argparse would print.
    Usage { message: String, prose: String },
}

pub struct Parser {
    prog: String,
    actions: Vec<Action>,
}

impl Parser {
    /// `argparse.ArgumentParser(prog=...)`, with `-h` first as argparse adds
    /// it, then `--json` as `cli._parser` registers it.
    pub fn new(prog: &str) -> Parser {
        let mut parser = Parser {
            prog: prog.into(),
            actions: Vec::new(),
        };
        parser.actions.push(Action {
            options: vec!["-h", "--help"],
            dest: "help".into(),
            nargs: Nargs::Zero,
            kind: Kind::Help,
            metavar: None,
            constant: None,
            default: None,
            help: Some("show this help message and exit"),
        });
        parser.flag("--json", None);
        parser
    }

    fn dest_of(option: &str) -> String {
        option.trim_start_matches('-').replace('-', "_")
    }

    /// `add_argument("--name")`.
    pub fn option(&mut self, option: &'static str) -> &mut Self {
        self.push(option, Nargs::One, None, None, None)
    }

    /// `add_argument("--name", default=...)`.
    pub fn option_default(&mut self, option: &'static str, default: &'static str) -> &mut Self {
        self.push(option, Nargs::One, None, None, Some(default))
    }

    /// `add_argument("--name", metavar=...)`.
    pub fn option_metavar(&mut self, option: &'static str, metavar: &'static str) -> &mut Self {
        self.push(option, Nargs::One, Some(metavar), None, None)
    }

    /// `add_argument("--name", nargs="?", const=..., metavar=...)`.
    pub fn option_optional(
        &mut self,
        option: &'static str,
        constant: &'static str,
        metavar: &'static str,
    ) -> &mut Self {
        self.push(option, Nargs::Optional, Some(metavar), Some(constant), None)
    }

    fn push(
        &mut self,
        option: &'static str,
        nargs: Nargs,
        metavar: Option<&'static str>,
        constant: Option<&'static str>,
        default: Option<&'static str>,
    ) -> &mut Self {
        self.actions.push(Action {
            options: vec![option],
            dest: Self::dest_of(option),
            nargs,
            kind: Kind::Store,
            metavar,
            constant,
            default,
            help: None,
        });
        self
    }

    /// `add_argument("--name", action="store_true"[, dest=...])`.
    pub fn flag(&mut self, option: &'static str, dest: Option<&str>) -> &mut Self {
        self.actions.push(Action {
            options: vec![option],
            dest: dest
                .map(str::to_string)
                .unwrap_or_else(|| Self::dest_of(option)),
            nargs: Nargs::Zero,
            kind: Kind::StoreTrue,
            metavar: None,
            constant: None,
            default: None,
            help: None,
        });
        self
    }

    /// `add_argument("name"[, nargs="?"])`.
    pub fn positional(&mut self, name: &'static str, nargs: Nargs) -> &mut Self {
        self.actions.push(Action {
            options: Vec::new(),
            dest: name.into(),
            nargs,
            kind: Kind::Store,
            metavar: None,
            constant: None,
            default: None,
            help: None,
        });
        self
    }

    /// A usage error a command found after parsing, such as two flags that
    /// exclude each other: `argument --b: not allowed with argument --a`.
    pub fn usage_error(&self, message: &str) -> Exit {
        self.error(message.into())
    }

    fn error(&self, message: String) -> Exit {
        let prose = format!("{}{}: error: {}\n", self.format_usage(), self.prog, message);
        Exit::Usage { message, prose }
    }

    fn argument_error(&self, action: Option<&Action>, message: &str) -> Exit {
        match action {
            Some(action) => self.error(format!("argument {}: {}", action.name(), message)),
            None => self.error(message.into()),
        }
    }

    fn lookup(&self, option: &str) -> Option<usize> {
        self.actions
            .iter()
            .position(|action| action.options.contains(&option))
    }

    /// The option strings in registration order, as `_option_string_actions`
    /// iterates them.
    fn option_strings(&self) -> Vec<(&'static str, usize)> {
        self.actions
            .iter()
            .enumerate()
            .flat_map(|(index, action)| action.options.iter().map(move |o| (*o, index)))
            .collect()
    }

    /// `_parse_optional`: None for a positional, else every interpretation.
    fn parse_optional(&self, arg: &str) -> Option<Vec<Tuple>> {
        if arg.is_empty() || !arg.starts_with('-') {
            return None;
        }
        if let Some(index) = self.lookup(arg) {
            return Some(vec![Tuple {
                action: Some(index),
                option: arg.into(),
                sep: None,
                explicit: None,
            }]);
        }
        if arg.chars().count() == 1 {
            return None;
        }
        if let Some((option, explicit)) = arg.split_once('=') {
            if let Some(index) = self.lookup(option) {
                return Some(vec![Tuple {
                    action: Some(index),
                    option: option.into(),
                    sep: Some("=".into()),
                    explicit: Some(explicit.into()),
                }]);
            }
        }
        let tuples = self.option_tuples(arg);
        if !tuples.is_empty() {
            return Some(tuples);
        }
        if is_negative_number(arg) {
            return None;
        }
        if arg.contains(' ') {
            return None;
        }
        Some(vec![Tuple {
            action: None,
            option: arg.into(),
            sep: None,
            explicit: None,
        }])
    }

    fn option_tuples(&self, arg: &str) -> Vec<Tuple> {
        let mut result = Vec::new();
        let second_dash = arg.chars().nth(1) == Some('-');
        let (prefix, sep, explicit) = match arg.split_once('=') {
            Some((prefix, explicit)) => (prefix, Some("=".to_string()), Some(explicit.to_string())),
            None => (arg, None, None),
        };
        if second_dash {
            for (option, index) in self.option_strings() {
                if option.starts_with(prefix) {
                    result.push(Tuple {
                        action: Some(index),
                        option: option.into(),
                        sep: sep.clone(),
                        explicit: explicit.clone(),
                    });
                }
            }
        } else {
            let split = arg
                .char_indices()
                .nth(2)
                .map(|(i, _)| i)
                .unwrap_or(arg.len());
            let (short, short_explicit) = arg.split_at(split);
            for (option, index) in self.option_strings() {
                if option == short {
                    result.push(Tuple {
                        action: Some(index),
                        option: option.into(),
                        sep: Some(String::new()),
                        explicit: Some(short_explicit.into()),
                    });
                } else if option.starts_with(prefix) {
                    result.push(Tuple {
                        action: Some(index),
                        option: option.into(),
                        sep: sep.clone(),
                        explicit: explicit.clone(),
                    });
                }
            }
        }
        result
    }

    pub fn parse(&self, argv: &[String]) -> Result<Args, Exit> {
        let mut pattern = String::new();
        let mut options: HashMap<usize, Vec<Tuple>> = HashMap::new();
        let mut after_dashes = false;
        for (i, arg) in argv.iter().enumerate() {
            if after_dashes {
                pattern.push('A');
            } else if arg == "--" {
                pattern.push('-');
                after_dashes = true;
            } else {
                match self.parse_optional(arg) {
                    None => pattern.push('A'),
                    Some(tuples) => {
                        options.insert(i, tuples);
                        pattern.push('O');
                    }
                }
            }
        }
        let pattern: Vec<char> = pattern.chars().collect();

        let mut state = State {
            values: HashMap::new(),
            seen: vec![false; self.actions.len()],
            extras: Vec::new(),
        };
        for action in &self.actions {
            let default = match action.kind {
                Kind::StoreTrue => Val::Flag(false),
                Kind::Help => continue,
                Kind::Store => Val::Str(action.default.map(str::to_string)),
            };
            state.values.insert(action.dest.clone(), default);
        }
        let mut positionals: Vec<usize> = (0..self.actions.len())
            .filter(|&i| self.actions[i].options.is_empty())
            .collect();

        let max_option = options.keys().copied().max();
        let mut start = 0usize;
        if let Some(max_option) = max_option {
            while start <= max_option {
                let next = *options
                    .keys()
                    .filter(|&&i| i >= start)
                    .min()
                    .expect("an option remains");
                if start != next {
                    let end = self.consume_positionals(
                        &mut state,
                        &mut positionals,
                        argv,
                        &pattern,
                        start,
                    );
                    if end > start {
                        start = end;
                        continue;
                    }
                    start = end;
                }
                if !options.contains_key(&start) {
                    state.extras.extend(argv[start..next].iter().cloned());
                    start = next;
                }
                start = self.consume_optional(&mut state, argv, &pattern, &options, start)?;
            }
        }
        let stop = self.consume_positionals(&mut state, &mut positionals, argv, &pattern, start);
        state
            .extras
            .extend(argv[stop.min(argv.len())..].iter().cloned());

        let missing: Vec<String> = self
            .actions
            .iter()
            .enumerate()
            .filter(|(i, action)| !state.seen[*i] && action.required())
            .map(|(_, action)| action.name())
            .collect();
        if !missing.is_empty() {
            return Err(self.error(format!(
                "the following arguments are required: {}",
                missing.join(", ")
            )));
        }
        if !state.extras.is_empty() {
            return Err(self.error(format!(
                "unrecognized arguments: {}",
                state.extras.join(" ")
            )));
        }
        Ok(Args {
            values: state.values,
        })
    }

    fn take(&self, state: &mut State, index: usize, mut args: Vec<String>) -> Result<(), Exit> {
        state.seen[index] = true;
        let action = &self.actions[index];
        match action.kind {
            Kind::Help => Err(Exit::Help(self.format_help())),
            Kind::StoreTrue => {
                state.values.insert(action.dest.clone(), Val::Flag(true));
                Ok(())
            }
            Kind::Store => {
                let value = if args.is_empty() {
                    if action.options.is_empty() {
                        action.default.map(str::to_string)
                    } else {
                        action.constant.map(str::to_string)
                    }
                } else {
                    Some(args.remove(0))
                };
                state.values.insert(action.dest.clone(), Val::Str(value));
                Ok(())
            }
        }
    }

    /// `_match_argument` for an optional: how many of the following strings
    /// it takes, or argparse's error.
    fn match_option(&self, index: usize, following: &[char]) -> Result<usize, Exit> {
        let action = &self.actions[index];
        match action.nargs {
            Nargs::Zero => Ok(0),
            Nargs::Optional => Ok(usize::from(following.first() == Some(&'A'))),
            Nargs::One if following.first() == Some(&'A') => Ok(1),
            Nargs::One => Err(self.argument_error(Some(action), "expected one argument")),
        }
    }

    fn consume_optional(
        &self,
        state: &mut State,
        argv: &[String],
        pattern: &[char],
        options: &HashMap<usize, Vec<Tuple>>,
        start: usize,
    ) -> Result<usize, Exit> {
        let tuples = &options[&start];
        if tuples.len() > 1 {
            let matches: Vec<&str> = tuples.iter().map(|t| t.option.as_str()).collect();
            return Err(self.argument_error(
                None,
                &format!(
                    "ambiguous option: {} could match {}",
                    argv[start],
                    matches.join(", ")
                ),
            ));
        }
        let Tuple {
            action,
            mut option,
            mut sep,
            mut explicit,
        } = tuples[0].clone();
        let mut action = action;
        let mut taken: Vec<(usize, Vec<String>)> = Vec::new();
        let stop;
        loop {
            let Some(index) = action else {
                state.extras.push(argv[start].clone());
                // Actions gathered so far from a -xyz cluster still apply.
                for (index, args) in taken {
                    self.take(state, index, args)?;
                }
                return Ok(start + 1);
            };
            if let Some(arg) = explicit.clone() {
                let count = self.match_option(index, &['A'])?;
                let single_dash = option.chars().nth(1) != Some('-');
                if count == 0 && single_dash && !arg.is_empty() {
                    if sep.as_deref().is_some_and(|s| !s.is_empty()) || arg.starts_with('-') {
                        return Err(self.argument_error(
                            Some(&self.actions[index]),
                            &format!("ignored explicit argument {}", py_repr(&arg)),
                        ));
                    }
                    taken.push((index, Vec::new()));
                    let first: String = arg.chars().next().into_iter().collect();
                    option = format!("-{}", first);
                    let rest: String = arg.chars().skip(1).collect();
                    match self.lookup(&option) {
                        Some(next) => {
                            action = Some(next);
                            if rest.is_empty() {
                                sep = None;
                                explicit = None;
                            } else if let Some(after) = rest.strip_prefix('=') {
                                sep = Some("=".into());
                                explicit = Some(after.into());
                            } else {
                                sep = Some(String::new());
                                explicit = Some(rest);
                            }
                        }
                        None => {
                            state.extras.push(format!("-{}", arg));
                            stop = start + 1;
                            break;
                        }
                    }
                } else if count == 1 {
                    taken.push((index, vec![arg]));
                    stop = start + 1;
                    break;
                } else {
                    return Err(self.argument_error(
                        Some(&self.actions[index]),
                        &format!("ignored explicit argument {}", py_repr(&arg)),
                    ));
                }
            } else {
                let from = start + 1;
                let count = self.match_option(index, &pattern[from.min(pattern.len())..])?;
                taken.push((index, argv[from..from + count].to_vec()));
                stop = from + count;
                break;
            }
        }
        for (index, args) in taken {
            self.take(state, index, args)?;
        }
        Ok(stop)
    }

    fn consume_positionals(
        &self,
        state: &mut State,
        positionals: &mut Vec<usize>,
        argv: &[String],
        pattern: &[char],
        start: usize,
    ) -> usize {
        let selected = &pattern[start.min(pattern.len())..];
        let counts = self.match_partial(positionals, selected);
        let mut index = start;
        for (&action, &count) in positionals.iter().zip(&counts) {
            let mut args = argv[index..index + count].to_vec();
            if pattern[index..index + count].contains(&'-') {
                if let Some(at) = args.iter().position(|a| a == "--") {
                    args.remove(at);
                }
            }
            index += count;
            // Positionals never raise: they are plain stores.
            let _ = self.take(state, action, args);
        }
        positionals.drain(..counts.len());
        index
    }

    /// `_match_arguments_partial`: the longest prefix of positionals that
    /// matches, each one's count, then trailing empties trimmed when an
    /// option follows — the 3.12.10 behaviour that stops a `nargs="?"`
    /// positional being consumed empty ahead of an option.
    fn match_partial(&self, positionals: &[usize], pattern: &[char]) -> Vec<usize> {
        for i in (1..=positionals.len()).rev() {
            let shapes: Vec<Nargs> = positionals[..i]
                .iter()
                .map(|&p| self.actions[p].nargs)
                .collect();
            if let Some(mut counts) = match_groups(&shapes, pattern, 0) {
                let end: usize = counts.iter().sum();
                if end < pattern.len() && pattern[end] == 'O' {
                    while counts.last() == Some(&0) {
                        counts.pop();
                    }
                }
                return counts;
            }
        }
        Vec::new()
    }

    // -- formatting -----------------------------------------------------------

    fn width() -> usize {
        terminal_columns().saturating_sub(2)
    }

    pub fn format_usage(&self) -> String {
        format!(
            "{}\n",
            self.usage_block(Self::width()).trim_end_matches('\n')
        )
    }

    fn usage_block(&self, width: usize) -> String {
        let prefix = "usage: ";
        let optionals: Vec<&Action> = self
            .actions
            .iter()
            .filter(|a| !a.options.is_empty())
            .collect();
        let positionals: Vec<&Action> = self
            .actions
            .iter()
            .filter(|a| a.options.is_empty())
            .collect();
        let opt_parts: Vec<String> = optionals.iter().map(|a| a.usage()).collect();
        let pos_parts: Vec<String> = positionals.iter().map(|a| a.usage()).collect();
        let all: Vec<String> = opt_parts.iter().chain(&pos_parts).cloned().collect();
        let mut usage = self.prog.clone();
        if !all.is_empty() {
            usage = format!("{} {}", usage, all.join(" "));
        }
        let text_width = width;
        if prefix.len() + usage.chars().count() > text_width {
            let get_lines = |parts: &[String], indent: &str, prefix: Option<&str>| -> Vec<String> {
                let mut lines = Vec::new();
                let mut line: Vec<&str> = Vec::new();
                let indent_length = indent.chars().count();
                let mut line_len = match prefix {
                    Some(p) => p.chars().count() as isize - 1,
                    None => indent_length as isize - 1,
                };
                for part in parts {
                    let part_len = part.chars().count() as isize;
                    if line_len + 1 + part_len > text_width as isize && !line.is_empty() {
                        lines.push(format!("{}{}", indent, line.join(" ")));
                        line.clear();
                        line_len = indent_length as isize - 1;
                    }
                    line.push(part);
                    line_len += part_len + 1;
                }
                if !line.is_empty() {
                    lines.push(format!("{}{}", indent, line.join(" ")));
                }
                if prefix.is_some() {
                    lines[0] = lines[0].chars().skip(indent_length).collect();
                }
                lines
            };
            let prog_len = self.prog.chars().count();
            let lines: Vec<String> = if (prefix.len() + prog_len) as f64 <= 0.75 * text_width as f64
            {
                let indent = " ".repeat(prefix.len() + prog_len + 1);
                let mut head = vec![self.prog.clone()];
                if !opt_parts.is_empty() {
                    head.extend(opt_parts.iter().cloned());
                    let mut lines = get_lines(&head, &indent, Some(prefix));
                    lines.extend(get_lines(&pos_parts, &indent, None));
                    lines
                } else if !pos_parts.is_empty() {
                    head.extend(pos_parts.iter().cloned());
                    get_lines(&head, &indent, Some(prefix))
                } else {
                    head
                }
            } else {
                let indent = " ".repeat(prefix.len());
                let parts: Vec<String> = opt_parts.iter().chain(&pos_parts).cloned().collect();
                let mut lines = get_lines(&parts, &indent, None);
                if lines.len() > 1 {
                    lines = get_lines(&opt_parts, &indent, None);
                    lines.extend(get_lines(&pos_parts, &indent, None));
                }
                let mut all = vec![self.prog.clone()];
                all.extend(lines);
                all
            };
            usage = lines.join("\n");
        }
        format!("{}{}\n\n", prefix, usage)
    }

    pub fn format_help(&self) -> String {
        let width = Self::width();
        let max_help_position = 24usize.min(width.saturating_sub(20).max(4));
        let indent = 2usize;
        let action_max = self
            .actions
            .iter()
            .map(|a| a.invocation().chars().count() + indent)
            .max()
            .unwrap_or(0);
        let help_position = (action_max + 2).min(max_help_position);
        let help_width = width.saturating_sub(help_position).max(11);
        let action_width = help_position as isize - indent as isize - 2;

        let format_action = |action: &Action| -> String {
            let header = action.invocation();
            let mut out = String::new();
            match action.help {
                None => out.push_str(&format!("{}{}\n", " ".repeat(indent), header)),
                Some(help) => {
                    let fits = header.chars().count() as isize <= action_width;
                    let first_indent = if fits {
                        let pad = action_width as usize - header.chars().count();
                        out.push_str(&format!(
                            "{}{}{}  ",
                            " ".repeat(indent),
                            header,
                            " ".repeat(pad)
                        ));
                        0
                    } else {
                        out.push_str(&format!("{}{}\n", " ".repeat(indent), header));
                        help_position
                    };
                    let lines = wrap(help, help_width);
                    out.push_str(&format!("{}{}\n", " ".repeat(first_indent), lines[0]));
                    for line in &lines[1..] {
                        out.push_str(&format!("{}{}\n", " ".repeat(help_position), line));
                    }
                }
            }
            out
        };
        let section = |heading: &str, actions: Vec<&Action>| -> String {
            if actions.is_empty() {
                return String::new();
            }
            let body: String = actions.into_iter().map(&format_action).collect();
            format!("\n{}:\n{}\n", heading, body)
        };
        let mut help = self.usage_block(width);
        help.push_str(&section(
            "positional arguments",
            self.actions
                .iter()
                .filter(|a| a.options.is_empty())
                .collect(),
        ));
        help.push_str(&section(
            "options",
            self.actions
                .iter()
                .filter(|a| !a.options.is_empty())
                .collect(),
        ));
        // _long_break_matcher, then strip('\n') + '\n'.
        while help.contains("\n\n\n") {
            help = help.replace("\n\n\n", "\n\n");
        }
        format!("{}\n", help.trim_matches('\n'))
    }
}

#[derive(Clone)]
struct Tuple {
    action: Option<usize>,
    option: String,
    sep: Option<String>,
    explicit: Option<String>,
}

struct State {
    values: HashMap<String, Val>,
    seen: Vec<bool>,
    extras: Vec<String>,
}

/// `re.match` of the concatenated positional patterns — each `(-*A-*)` or
/// `(-*A?-*)` — with the backtracking order a regex engine uses: greedy
/// first, shorter on failure.
fn match_groups(shapes: &[Nargs], pattern: &[char], at: usize) -> Option<Vec<usize>> {
    let Some((&shape, rest)) = shapes.split_first() else {
        return Some(Vec::new());
    };
    let dashes = |from: usize| pattern[from..].iter().take_while(|&&c| c == '-').count();
    let lead_max = dashes(at);
    for lead in (0..=lead_max).rev() {
        let after_lead = at + lead;
        let a_options: &[usize] = match shape {
            Nargs::One => &[1],
            Nargs::Optional => &[1, 0],
            Nargs::Zero => &[0],
        };
        for &a in a_options {
            if a == 1 && pattern.get(after_lead) != Some(&'A') {
                continue;
            }
            let after_a = after_lead + a;
            let trail_max = dashes(after_a);
            for trail in (0..=trail_max).rev() {
                let end = after_a + trail;
                if let Some(mut counts) = match_groups(rest, pattern, end) {
                    counts.insert(0, end - at);
                    return Some(counts);
                }
            }
        }
    }
    None
}

/// `^-\d+$|^-\d*\.\d+$`.
fn is_negative_number(arg: &str) -> bool {
    let Some(body) = arg.strip_prefix('-') else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if digits(body) {
        return true;
    }
    match body.split_once('.') {
        Some((whole, fraction)) => (whole.is_empty() || digits(whole)) && digits(fraction),
        None => false,
    }
}

/// Python's `repr()` of a str, as `%r` puts it in "ignored explicit argument".
fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `textwrap.wrap` for help text made of short words.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// `shutil.get_terminal_size().columns`: COLUMNS if it is a positive integer,
/// else the size of the terminal on stdout, else 80.
pub fn terminal_columns() -> usize {
    if let Some(columns) = std::env::var("COLUMNS")
        .ok()
        .and_then(|value| value.trim().parse::<i64>().ok())
        .filter(|&columns| columns > 0)
    {
        return columns as usize;
    }
    stdout_columns().filter(|&c| c > 0).unwrap_or(80)
}

#[cfg(unix)]
fn stdout_columns() -> Option<usize> {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ writes a winsize into the pointer it is given.
    let result = unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut size) };
    (result == 0).then_some(size.ws_col as usize)
}

#[cfg(not(unix))]
fn stdout_columns() -> Option<usize> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_trailing_optional_positional_is_not_swallowed_before_an_option() {
        let mut parser = Parser::new("loch ask");
        parser
            .positional("target", Nargs::One)
            .positional("question", Nargs::Optional)
            .flag("--at-commit", None);
        let args = parser
            .parse(&argv(&["t", "--at-commit", "q"]))
            .ok()
            .unwrap();
        assert_eq!(args.str("target").as_deref(), Some("t"));
        assert_eq!(args.str("question").as_deref(), Some("q"));
        assert!(args.flag("at_commit"));
    }

    #[test]
    fn negative_numbers() {
        assert!(is_negative_number("-5"));
        assert!(is_negative_number("-.5"));
        assert!(!is_negative_number("-x"));
    }
}
