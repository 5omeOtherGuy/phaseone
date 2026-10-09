//! ADR-0118 Decision 2: may a `shell` command overlap the other reads of its response?
//!
//! The classifier answers yes only for a command line built from the read-only allow-list,
//! each command with options from its own allowed-option list, joined by `|`, `;`, `&&` or
//! `||`, with no redirection, substitution, subshell, brace group, background `&`,
//! assignment prefix, expansion, comment or line break. Any doubt, a lexing failure
//! included, means no: the call then runs alone, which is what every call did before.
//!
//! A misclassified command can at worst race other reads of the same agent: a writing call
//! is always a group of its own (ADR-0118 Decision 4). So this is a careful lexer of what
//! the line says, not a shell: a quoted operator is a literal, an unquoted one decides.

/// One command's allowed options, as data (ADR-0118 Decision 2): every option of a listed
/// command must be on its own list. An option that writes, or runs a program, is on none.
struct Options {
    /// Short flags without an argument; several may share one `-` (`ls -la`).
    flags: &'static str,
    /// Short flags taking an argument, attached (`-n5`) or as the next word (`-n 5`).
    with_value: &'static str,
    /// Short flags whose argument is optional and only ever attached (`git diff -M50%`).
    attached: &'static str,
    /// Long options without a value.
    long: &'static [&'static str],
    /// Long options taking a value, `--name=value` or `--name value`.
    long_value: &'static [&'static str],
    /// Long options whose value is optional and only ever `--name=value`.
    long_optional: &'static [&'static str],
    /// `-<digits>` is a count (`head -5`, `grep -3`, `git log -5`).
    numeric: bool,
    /// The most operands the command may have; `uniq` writes its second one.
    max_operands: Option<usize>,
}

const NONE: Options = Options {
    flags: "",
    with_value: "",
    attached: "",
    long: &[],
    long_value: &[],
    long_optional: &[],
    numeric: false,
    max_operands: None,
};

const LS: Options = Options {
    flags: "aAbBcCdfFgGhHiklLmnNopqQrRsStuUvxXZ1",
    with_value: "wIT",
    long: &[
        "all",
        "almost-all",
        "author",
        "escape",
        "directory",
        "file-type",
        "full-time",
        "group-directories-first",
        "no-group",
        "human-readable",
        "si",
        "dereference-command-line",
        "dereference-command-line-symlink-to-dir",
        "inode",
        "kibibytes",
        "dereference",
        "literal",
        "numeric-uid-gid",
        "hide-control-chars",
        "show-control-chars",
        "quote-name",
        "reverse",
        "recursive",
        "size",
        "context",
        "zero",
        "ignore-backups",
    ],
    long_value: &[
        "hide",
        "indicator-style",
        "ignore",
        "quoting-style",
        "sort",
        "time",
        "time-style",
        "tabsize",
        "width",
        "block-size",
        "format",
    ],
    long_optional: &["color", "classify", "hyperlink"],
    ..NONE
};

const CAT: Options = Options {
    flags: "AbeEnstTuv",
    long: &[
        "show-all",
        "number-nonblank",
        "show-ends",
        "number",
        "squeeze-blank",
        "show-tabs",
        "show-nonprinting",
    ],
    ..NONE
};

const HEAD_TAIL: Options = Options {
    flags: "qvz",
    with_value: "nc",
    long: &["quiet", "silent", "verbose", "zero-terminated"],
    long_value: &["lines", "bytes"],
    numeric: true,
    ..NONE
};

const GREP: Options = Options {
    flags: "EFGPiyvwxclLnhHoqsrRaIUzZbT",
    with_value: "efmABCdD",
    long: &[
        "extended-regexp",
        "fixed-strings",
        "basic-regexp",
        "perl-regexp",
        "ignore-case",
        "no-ignore-case",
        "invert-match",
        "word-regexp",
        "line-regexp",
        "count",
        "files-with-matches",
        "files-without-match",
        "line-number",
        "no-filename",
        "with-filename",
        "only-matching",
        "quiet",
        "silent",
        "no-messages",
        "recursive",
        "dereference-recursive",
        "text",
        "byte-offset",
        "initial-tab",
        "null",
        "null-data",
        "line-buffered",
    ],
    long_value: &[
        "regexp",
        "file",
        "max-count",
        "after-context",
        "before-context",
        "context",
        "include",
        "exclude",
        "exclude-dir",
        "exclude-from",
        "label",
        "binary-files",
        "devices",
        "directories",
    ],
    long_optional: &["color", "colour"],
    numeric: true,
    ..NONE
};

/// `rg` without `-z`/`--search-zip`, `--pre` and `--pre-glob`, which run other programs.
const RG: Options = Options {
    flags: "iISsvwxclnNHoquaLpFPUb0.",
    with_value: "efgtTmABCMjrEd",
    long: &[
        "files",
        "files-with-matches",
        "files-without-match",
        "count",
        "count-matches",
        "fixed-strings",
        "ignore-case",
        "smart-case",
        "case-sensitive",
        "word-regexp",
        "line-regexp",
        "invert-match",
        "only-matching",
        "multiline",
        "multiline-dotall",
        "follow",
        "hidden",
        "no-hidden",
        "no-ignore",
        "no-ignore-vcs",
        "no-ignore-parent",
        "no-ignore-dot",
        "no-ignore-global",
        "no-ignore-exclude",
        "no-ignore-files",
        "no-ignore-messages",
        "no-messages",
        "unrestricted",
        "text",
        "binary",
        "heading",
        "no-heading",
        "line-number",
        "no-line-number",
        "column",
        "no-column",
        "with-filename",
        "no-filename",
        "null",
        "null-data",
        "quiet",
        "stats",
        "trim",
        "vimgrep",
        "json",
        "no-json",
        "pcre2",
        "no-pcre2",
        "passthru",
        "crlf",
        "no-config",
        "one-file-system",
        "glob-case-insensitive",
        "byte-offset",
        "block-buffered",
        "line-buffered",
        "type-list",
        "max-columns-preview",
        "include-zero",
        "no-require-git",
        "pretty",
    ],
    long_value: &[
        "regexp",
        "file",
        "glob",
        "iglob",
        "type",
        "type-not",
        "type-add",
        "type-clear",
        "max-count",
        "max-depth",
        "maxdepth",
        "max-filesize",
        "context",
        "after-context",
        "before-context",
        "replace",
        "encoding",
        "threads",
        "sort",
        "sortr",
        "colors",
        "path-separator",
        "engine",
        "ignore-file",
        "max-columns",
        "context-separator",
        "field-match-separator",
        "field-context-separator",
    ],
    long_optional: &["color"],
    ..NONE
};

const WC: Options = Options {
    flags: "cmlwL",
    long: &["bytes", "chars", "lines", "words", "max-line-length"],
    ..NONE
};

const STAT: Options = Options {
    flags: "Lft",
    with_value: "c",
    long: &["dereference", "file-system", "terse"],
    long_value: &["format", "printf", "cached"],
    ..NONE
};

/// `file` without `-C`/`--compile` (writes a magic file), `-z`/`-Z` (may run
/// decompressors) and `-p` (resets access times).
const FILE: Options = Options {
    flags: "biLhkNr0E",
    with_value: "eF",
    long: &[
        "brief",
        "mime",
        "mime-type",
        "mime-encoding",
        "dereference",
        "no-dereference",
        "keep-going",
        "no-pad",
        "raw",
        "print0",
        "extension",
        "apple",
    ],
    long_value: &["exclude", "exclude-quiet", "separator"],
    ..NONE
};

const PWD_CD: Options = Options {
    flags: "LP",
    ..NONE
};

const REALPATH: Options = Options {
    flags: "emLPqsz",
    long: &[
        "canonicalize-existing",
        "canonicalize-missing",
        "logical",
        "physical",
        "quiet",
        "strip",
        "no-symlinks",
        "zero",
    ],
    long_value: &["relative-to", "relative-base"],
    ..NONE
};

const BASENAME: Options = Options {
    flags: "az",
    with_value: "s",
    long: &["multiple", "zero"],
    long_value: &["suffix"],
    ..NONE
};

const DIRNAME: Options = Options {
    flags: "z",
    long: &["zero"],
    ..NONE
};

const DU: Options = Options {
    flags: "0abcDhHklLmPsSx",
    with_value: "dBt",
    long: &[
        "null",
        "all",
        "apparent-size",
        "bytes",
        "total",
        "dereference-args",
        "human-readable",
        "inodes",
        "count-links",
        "dereference",
        "no-dereference",
        "separate-dirs",
        "si",
        "summarize",
        "one-file-system",
    ],
    long_value: &[
        "max-depth",
        "block-size",
        "threshold",
        "exclude",
        "time-style",
    ],
    long_optional: &["time"],
    ..NONE
};

const CUT: Options = Options {
    flags: "nsz",
    with_value: "bcfd",
    long: &["complement", "only-delimited", "zero-terminated"],
    long_value: &[
        "bytes",
        "characters",
        "fields",
        "delimiter",
        "output-delimiter",
    ],
    ..NONE
};

const TR: Options = Options {
    flags: "cCdst",
    long: &["complement", "delete", "squeeze-repeats", "truncate-set1"],
    ..NONE
};

/// `sort` without `-o`/`--output`, `--compress-program`, `-T`/`--temporary-directory`,
/// `--files0-from` and `--random-source`.
const SORT: Options = Options {
    flags: "bdfghiMnRrVcCmsuz",
    with_value: "ktS",
    long: &[
        "ignore-leading-blanks",
        "dictionary-order",
        "ignore-case",
        "general-numeric-sort",
        "human-numeric-sort",
        "ignore-nonprinting",
        "month-sort",
        "numeric-sort",
        "random-sort",
        "reverse",
        "version-sort",
        "merge",
        "stable",
        "unique",
        "zero-terminated",
        "debug",
    ],
    long_value: &["key", "field-separator", "buffer-size", "sort", "parallel"],
    long_optional: &["check"],
    ..NONE
};

/// `uniq` with at most one operand: a second one is the file it writes.
const UNIQ: Options = Options {
    flags: "cdDiuz",
    with_value: "fsw",
    long: &[
        "count",
        "repeated",
        "ignore-case",
        "unique",
        "zero-terminated",
    ],
    long_value: &["skip-fields", "skip-chars", "check-chars"],
    long_optional: &["all-repeated", "group"],
    max_operands: Some(1),
    ..NONE
};

/// `tree` without `-o` (writes its listing to a file) and the HTML options.
const TREE: Options = Options {
    flags: "adlfxiqNQpugshDFvtcUrCnASJX",
    with_value: "LPI",
    long: &[
        "noreport",
        "dirsfirst",
        "prune",
        "gitignore",
        "matchdirs",
        "ignore-case",
        "du",
        "si",
        "inodes",
        "device",
    ],
    long_value: &["filelimit", "charset", "sort", "timefmt"],
    ..NONE
};

/// `git status`. It refreshes the index when it may; a Shared call runs with
/// `GIT_OPTIONAL_LOCKS=0`, so it does not ([`crate::GIT_OPTIONAL_LOCKS_PREFIX`]).
const GIT_STATUS: Options = Options {
    flags: "sbzv",
    attached: "u",
    long: &[
        "short",
        "branch",
        "long",
        "verbose",
        "no-column",
        "ahead-behind",
        "no-ahead-behind",
        "renames",
        "no-renames",
        "show-stash",
        "null",
    ],
    long_optional: &[
        "porcelain",
        "untracked-files",
        "ignored",
        "ignore-submodules",
        "column",
        "find-renames",
    ],
    ..NONE
};

/// `git log`, `git diff` and `git show`, without `--output`, `--ext-diff`, `--textconv`,
/// `--show-signature` (runs gpg) and `--no-index`.
const GIT_HISTORY: Options = Options {
    flags: "pusziwbRgv",
    with_value: "nSGLU",
    attached: "MCBl",
    long: &[
        "oneline",
        "shortstat",
        "numstat",
        "name-only",
        "name-status",
        "summary",
        "patch",
        "no-patch",
        "raw",
        "patch-with-stat",
        "patch-with-raw",
        "compact-summary",
        "graph",
        "no-decorate",
        "abbrev-commit",
        "no-abbrev-commit",
        "no-abbrev",
        "reverse",
        "first-parent",
        "merges",
        "no-merges",
        "follow",
        "pickaxe-all",
        "pickaxe-regex",
        "no-color",
        "cached",
        "staged",
        "merge-base",
        "ignore-all-space",
        "ignore-space-change",
        "ignore-blank-lines",
        "ignore-space-at-eol",
        "ignore-cr-at-eol",
        "minimal",
        "patience",
        "histogram",
        "find-copies-harder",
        "no-renames",
        "full-index",
        "binary",
        "check",
        "exit-code",
        "quiet",
        "no-ext-diff",
        "no-textconv",
        "text",
        "no-prefix",
        "left-right",
        "cherry-pick",
        "cherry-mark",
        "cherry",
        "boundary",
        "ancestry-path",
        "simplify-by-decoration",
        "topo-order",
        "date-order",
        "author-date-order",
        "source",
        "full-history",
        "parents",
        "children",
        "walk-reflogs",
        "regexp-ignore-case",
        "extended-regexp",
        "fixed-strings",
        "perl-regexp",
        "all-match",
        "invert-grep",
        "no-expand-tabs",
        "no-notes",
        "mailmap",
        "no-mailmap",
        "use-mailmap",
        "log-size",
    ],
    long_value: &[
        "unified",
        "max-count",
        "skip",
        "since",
        "until",
        "after",
        "before",
        "author",
        "committer",
        "grep",
        "format",
        "date",
        "diff-filter",
        "diff-algorithm",
        "word-diff-regex",
        "src-prefix",
        "dst-prefix",
        "line-prefix",
        "encoding",
        "stat-width",
        "stat-name-width",
        "stat-count",
        "max-parents",
        "min-parents",
        "exclude",
    ],
    long_optional: &[
        "stat",
        "all",
        "branches",
        "tags",
        "remotes",
        "decorate",
        "abbrev",
        "color",
        "word-diff",
        "color-words",
        "find-renames",
        "find-copies",
        "relative",
        "dirstat",
        "submodule",
        "ignore-submodules",
        "pretty",
        "no-walk",
        "expand-tabs",
        "notes",
    ],
    numeric: true,
    ..NONE
};

/// `find` tests, options and actions without an argument; `-exec`, `-execdir`, `-ok`,
/// `-okdir`, `-delete`, `-fls` and `-fprint*` are on neither list.
const FIND_FLAGS: &[&str] = &[
    "-L",
    "-H",
    "-P",
    "-depth",
    "-d",
    "-mount",
    "-xdev",
    "-follow",
    "-noleaf",
    "-daystart",
    "-ignore_readdir_race",
    "-noignore_readdir_race",
    "-warn",
    "-nowarn",
    "-nouser",
    "-nogroup",
    "-empty",
    "-readable",
    "-writable",
    "-executable",
    "-print",
    "-print0",
    "-ls",
    "-prune",
    "-quit",
    "-true",
    "-false",
    "-not",
    "-and",
    "-or",
    "-a",
    "-o",
    "!",
    "(",
    ")",
    ",",
];

/// `find` tests and options that take the next word as their argument.
const FIND_WITH_VALUE: &[&str] = &[
    "-name",
    "-iname",
    "-path",
    "-ipath",
    "-wholename",
    "-iwholename",
    "-regex",
    "-iregex",
    "-regextype",
    "-type",
    "-xtype",
    "-size",
    "-mtime",
    "-mmin",
    "-atime",
    "-amin",
    "-ctime",
    "-cmin",
    "-newer",
    "-anewer",
    "-cnewer",
    "-newermt",
    "-perm",
    "-user",
    "-group",
    "-uid",
    "-gid",
    "-links",
    "-inum",
    "-samefile",
    "-maxdepth",
    "-mindepth",
    "-printf",
    "-lname",
    "-ilname",
    "-fstype",
    "-used",
];

/// The read-only allow-list of ADR-0118 Decision 2 and each command's option list.
fn options_of(command: &str) -> Option<&'static Options> {
    Some(match command {
        "ls" => &LS,
        "cat" => &CAT,
        "head" | "tail" => &HEAD_TAIL,
        "grep" => &GREP,
        "rg" => &RG,
        "wc" => &WC,
        "stat" => &STAT,
        "file" => &FILE,
        "pwd" | "cd" => &PWD_CD,
        "realpath" => &REALPATH,
        "basename" => &BASENAME,
        "dirname" => &DIRNAME,
        "du" => &DU,
        "cut" => &CUT,
        "tr" => &TR,
        "sort" => &SORT,
        "uniq" => &UNIQ,
        "tree" => &TREE,
        _ => return None,
    })
}

/// Whether `command` may overlap other reads: every command of the line is allowed.
pub(crate) fn is_read_only(command: &str) -> bool {
    match split(command) {
        Some(commands) => commands.iter().all(|words| allowed(words)),
        None => false,
    }
}

/// One command with its words: allowed when its name is listed and every option is on that
/// command's own list.
fn allowed(words: &[Word]) -> bool {
    let Some((name, args)) = words.split_first() else {
        return false;
    };
    if name.assignment {
        return false;
    }
    let args: Vec<&str> = args.iter().map(|word| word.text.as_str()).collect();
    match name.text.as_str() {
        "find" => find_allowed(&args),
        "git" => git_allowed(&args),
        command => options_of(command).is_some_and(|options| options_allowed(options, &args)),
    }
}

/// Every option on `options`' lists, and no more operands than it allows.
fn options_allowed(options: &Options, args: &[&str]) -> bool {
    let mut operands = 0;
    let mut only_operands = false;
    let mut words = args.iter();
    while let Some(&word) = words.next() {
        if only_operands || word == "-" || !word.starts_with('-') {
            operands += 1;
            continue;
        }
        if word == "--" {
            only_operands = true;
            continue;
        }
        if let Some(long) = word.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (long, None),
            };
            let known = if options.long_value.contains(&name) {
                value.is_some() || words.next().is_some()
            } else if options.long_optional.contains(&name) {
                true
            } else {
                value.is_none() && options.long.contains(&name)
            };
            if !known {
                return false;
            }
            continue;
        }
        let flags = &word[1..];
        if options.numeric && flags.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        for (at, flag) in flags.char_indices() {
            let rest = &flags[at + flag.len_utf8()..];
            if options.with_value.contains(flag) {
                if rest.is_empty() && words.next().is_none() {
                    return false;
                }
                break;
            }
            if options.attached.contains(flag) {
                break;
            }
            if !options.flags.contains(flag) {
                return false;
            }
        }
    }
    options.max_operands.is_none_or(|max| operands <= max)
}

/// `find`: every word that looks like an option, test or action is on its lists; the
/// argument of a test that takes one is skipped whatever it looks like.
fn find_allowed(args: &[&str]) -> bool {
    let mut words = args.iter();
    while let Some(&word) = words.next() {
        if FIND_WITH_VALUE.contains(&word) {
            if words.next().is_none() {
                return false;
            }
        } else if (word.starts_with('-') || FIND_FLAGS.contains(&word))
            && !FIND_FLAGS.contains(&word)
        {
            return false;
        }
    }
    true
}

/// `git`: global options only `-C <dir>` and `--no-pager`, then exactly one of `status`,
/// `log`, `diff`, `show` with that subcommand's allowed options.
fn git_allowed(args: &[&str]) -> bool {
    let mut words = args.iter();
    let subcommand = loop {
        match words.next() {
            Some(&"--no-pager") => continue,
            Some(&"-C") => {
                if words.next().is_none() {
                    return false;
                }
            }
            Some(&word) if !word.starts_with('-') => break word,
            _ => return false,
        }
    };
    let rest: Vec<&str> = words.copied().collect();
    match subcommand {
        "status" => options_allowed(&GIT_STATUS, &rest),
        "log" | "diff" | "show" => options_allowed(&GIT_HISTORY, &rest),
        _ => false,
    }
}

/// One word of a command, after quote removal.
#[derive(Default)]
struct Word {
    text: String,
    /// An unquoted `NAME=` starts it: in command position, an assignment prefix.
    assignment: bool,
}

/// What one lexed word is still being built from.
#[derive(Default)]
struct Builder {
    word: Word,
    /// Something, perhaps an empty quoted string, was written: the word exists.
    started: bool,
    /// Every character so far was unquoted, so a following `=` makes an assignment.
    plain: bool,
}

impl Builder {
    fn push(&mut self, ch: char, quoted: bool) {
        if !self.started {
            self.plain = true;
        }
        self.started = true;
        if quoted {
            self.plain = false;
        }
        self.word.text.push(ch);
    }

    /// An unquoted `=` after a plain name: `NAME=value`.
    fn equals(&mut self) {
        if self.started && self.plain && is_name(&self.word.text) {
            self.word.assignment = true;
        }
        self.push('=', false);
    }

    fn quoted_start(&mut self) {
        self.started = true;
        self.plain = false;
    }

    fn take(&mut self) -> Option<Word> {
        let builder = std::mem::take(self);
        builder.started.then_some(builder.word)
    }
}

fn is_name(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

/// Whether `$` followed by `next` starts an expansion or a quoting form (`$(`, `${`,
/// `$NAME`, `$1`, `$@`, `$'…'`, `$"…"`): any of them is a doubt.
fn starts_expansion(next: Option<char>) -> bool {
    next.is_some_and(|ch| {
        ch == '('
            || ch == '{'
            || ch == '_'
            || ch == '\''
            || ch == '"'
            || ch.is_ascii_alphanumeric()
            || "@*#?$!-".contains(ch)
    })
}

/// The commands of a line joined only by `|`, `;`, `&&` or `||`, each as its words, or
/// `None` when the line holds anything else outside quotes or cannot be lexed.
fn split(line: &str) -> Option<Vec<Vec<Word>>> {
    let mut commands: Vec<Vec<Word>> = Vec::new();
    let mut words: Vec<Word> = Vec::new();
    let mut builder = Builder::default();
    let mut chars = line.trim().chars().peekable();
    // A command ends; an empty one (`;;`, `| x`, a dangling `&&`) is a doubt.
    let end_command = |words: &mut Vec<Word>, commands: &mut Vec<Vec<Word>>| -> Option<()> {
        if words.is_empty() {
            return None;
        }
        commands.push(std::mem::take(words));
        Some(())
    };
    while let Some(ch) = chars.next() {
        match ch {
            ' ' | '\t' => words.extend(builder.take()),
            '\'' => {
                builder.quoted_start();
                loop {
                    match chars.next()? {
                        '\'' => break,
                        inner => builder.push(inner, true),
                    }
                }
            }
            '"' => {
                builder.quoted_start();
                loop {
                    match chars.next()? {
                        '"' => break,
                        '`' => return None,
                        '$' if starts_expansion(chars.peek().copied())
                            && chars.peek() != Some(&'"') =>
                        {
                            return None;
                        }
                        '\\' => match chars.next()? {
                            escaped @ ('"' | '\\' | '$' | '`') => builder.push(escaped, true),
                            '\n' => {}
                            other => {
                                builder.push('\\', true);
                                builder.push(other, true);
                            }
                        },
                        inner => builder.push(inner, true),
                    }
                }
            }
            '\\' => match chars.next()? {
                '\n' => {}
                escaped => builder.push(escaped, true),
            },
            '$' if starts_expansion(chars.peek().copied()) => return None,
            '#' if !builder.started => return None,
            '=' => builder.equals(),
            '|' => {
                words.extend(builder.take());
                match chars.peek() {
                    Some('|') => {
                        chars.next();
                    }
                    Some('&') => return None,
                    _ => {}
                }
                end_command(&mut words, &mut commands)?;
            }
            '&' => {
                if chars.next() != Some('&') {
                    return None;
                }
                words.extend(builder.take());
                end_command(&mut words, &mut commands)?;
            }
            ';' => {
                if chars.peek() == Some(&';') {
                    return None;
                }
                words.extend(builder.take());
                end_command(&mut words, &mut commands)?;
            }
            '`' | '<' | '>' | '(' | ')' | '{' | '}' | '\n' | '\r' => return None,
            other => builder.push(other, false),
        }
    }
    words.extend(builder.take());
    end_command(&mut words, &mut commands)?;
    Some(commands)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared(command: &str) -> bool {
        is_read_only(command)
    }

    /// ADR-0118 test 11: each allow-listed command alone with permitted options, and piped
    /// or chained with other listed commands.
    #[test]
    fn listed_read_only_commands_are_shared() {
        for command in [
            "ls",
            "ls -la src",
            "ls --color=never -1 --sort=time",
            "cat Cargo.toml",
            "cat -n src/lib.rs",
            "head -n 20 src/lib.rs",
            "head -5 a b",
            "tail -n +3 file",
            "tail -c100 log.txt",
            "grep -rn 'fn main' src",
            "grep -A3 -e pattern --include='*.rs' .",
            "rg x",
            "rg -n --hidden -g '*.rs' 'fn main' crates",
            "rg -t rust --max-depth 3 TODO",
            "wc -l src/lib.rs",
            "stat -c %s file",
            "file -b --mime-type image.png",
            "pwd",
            "cd src",
            "realpath --relative-to=. src/lib.rs",
            "basename -s .rs src/lib.rs",
            "dirname src/lib.rs",
            "du -sh target",
            "du -d 1 -h .",
            "cut -d, -f1 data.csv",
            "tr a-z A-Z",
            "sort -rn counts",
            "sort -k 2 -t , data.csv",
            "uniq -c sorted.txt",
            "tree -L 2 src",
            "find . -name '*.rs'",
            "find src -type f -mtime -7 -size +10k -print",
            r"find . \( -name a -o -name b \) -not -path './target/*'",
            "git status",
            "git status -sb",
            "git status --porcelain=v2 -uno",
            "git log --oneline -n 20",
            "git log -5 --stat",
            "git --no-pager log --format='%H %s' --since=2.weeks",
            "git -C crates/p1-core diff --stat HEAD~1",
            "git diff --cached --name-only",
            "git diff -M50% -U5 main...HEAD -- src",
            "git show HEAD:src/lib.rs",
            "git show --stat abc123",
            // Piped and chained with other listed commands.
            "rg x | head",
            "cd src && ls",
            "git log --oneline | wc -l",
            "ls; pwd",
            "ls missing || pwd",
            "cat a | sort | uniq -c | sort -rn | head -n 5",
            // An operator inside quotes is a literal.
            "grep ';' f",
            "grep '|' f && rg \"a && b\" src",
            "rg 'fn main$' src",
            "rg \"end$\" src",
            "grep -- -x file",
        ] {
            assert!(shared(command), "{command} should be Shared");
        }
    }

    /// ADR-0118 test 12: one case per excluded construct.
    #[test]
    fn everything_else_is_exclusive() {
        for command in [
            // An unlisted command alone, piped into and chained with a listed one.
            "cargo test",
            "ls | xargs rm",
            "cat a | python3 -c 'print(1)'",
            "ls && make",
            "ls; rm -rf target",
            "make || ls",
            // find actions that run or write.
            "find . -exec rm {} ;",
            "find . -exec cat '{}' \\;",
            "find . -execdir cat x \\;",
            "find . -ok rm \\;",
            "find . -okdir rm \\;",
            "find . -delete",
            "find . -fls out",
            "find . -fprint out",
            "find . -fprint0 out",
            "find . -fprintf out %p",
            // Writing options and unlisted subcommands.
            "sed -i s/a/b/ f",
            "sort -o out in",
            "sort --output=out in",
            "sort --compress-program=x in",
            "uniq a b",
            "tree -o out",
            "file -C",
            "file --compile",
            "rg --pre x pattern",
            "rg --pre-glob '*.gz' pattern",
            "rg -z pattern",
            "rg --search-zip pattern",
            "git diff --output=out",
            "git diff --ext-diff",
            "git log -p --textconv",
            "git -c core.pager=x log",
            "git --git-dir=x log",
            "git apply patch",
            "git checkout main",
            "git",
            "git log --show-signature",
            // An option missing from the command's list.
            "ls --some-new-flag",
            "ls -W",
            "head --lines",
            "tail -f log",
            // Redirections and tee.
            "ls > out",
            "ls >> out",
            "cat <> f",
            "ls 2> err",
            "cat < f",
            "cat <<EOF",
            "ls 2>&1",
            "ls &> out",
            "ls |& head",
            "ls | tee out",
            // Substitutions.
            "cat $(ls)",
            "cat `ls`",
            "cat \"$(ls)\"",
            "cat \"`ls`\"",
            "diff <(ls a) <(ls b)",
            "ls >(cat)",
            "cat $FILE",
            "cat ${FILE}",
            "cat \"$HOME/x\"",
            "ls $'a\\nb'",
            // Subshell, brace group, background.
            "(ls)",
            "{ ls; }",
            "ls &",
            "ls & pwd",
            // Assignment prefix.
            "NAME=value ls",
            "LC_ALL=C sort f",
            // Lexing failures and line structure.
            "cat 'unbalanced",
            "cat \"unbalanced",
            "ls \\",
            "ls\npwd",
            "ls ;; pwd",
            "| ls",
            "ls &&",
            "ls # comment",
            "",
            "   ",
        ] {
            assert!(!shared(command), "{command:?} should be Exclusive");
        }
    }

    #[test]
    fn only_a_leading_unquoted_name_assignment_is_a_prefix() {
        assert!(shared("grep 'a=b' f"));
        assert!(shared("grep a=b f"));
        assert!(shared("git log --format=%H"));
        assert!(!shared("A=1 grep a f"));
        assert!(!shared("ls && A=1 pwd"));
        assert!(
            !shared("'A'=1 ls"),
            "a quoted name is not a listed command either"
        );
    }
}
