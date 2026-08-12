//! Templates as plain files with YAML frontmatter (SUM-08, issue #36).
//!
//! A template is a file at `~/.flyonthewall/templates/<slug>.md`: YAML
//! frontmatter carrying the metadata, a Markdown body carrying the prompt.
//! Files rather than database rows, because a file can be opened in any editor,
//! committed to git, diffed in a pull request and shared as a gist — none of
//! which an in-app template editor can offer, and all of which are the point of
//! shipping a local-first tool instead of renting one.
//!
//! # The one rule this module exists to enforce
//!
//! **A malformed template is an error, never a silent fallback to the default.**
//! Silently ignoring a user's template is the worst available behaviour: they
//! will read a summary that does not match their template, conclude the model
//! ignored them, and never learn that a typo three lines into the frontmatter is
//! why. So every failure carries a line number and, where it can, the key they
//! probably meant:
//!
//! ```text
//! standup.md line 7: unknown key `temperture`, did you mean `temperature`?
//! ```
//!
//! [`TemplateSet::load`] therefore fails the whole load rather than skipping the
//! bad file, and no code path in this module returns a default template in place
//! of one that failed to parse.
//!
//! # Why `saphyr` and not `serde_yaml` or `serde_yml`
//!
//! `serde_yaml` is unmaintained (archived by its author in 2024). `serde_yml`
//! is a maintained fork and would work, but it is a *serde* front end: the
//! errors it produces are serde's, and while it can report a line, it cannot
//! report "you probably meant `description`" without a second pass over the
//! document anyway. `saphyr` hands back a `MarkedYaml` tree where **every node
//! carries its own span**, which is what lets the error above name the offending
//! key's line rather than the line the deserializer happened to stop on. Since
//! the located, suggestive error *is* the feature here, the parser that makes it
//! cheap is the right one.
//!
//! The frontmatter schema is small and closed, so hand-walking the tree costs
//! about as much as the `#[derive(Deserialize)]` would have.
//!
//! # The body is still untrusted
//!
//! Everything in here — the body, a section heading, the `name` — ends up inside
//! [`crate::prompt::assemble`]'s quarantine delimiter, and none of it can
//! override the grounding contract (spec 8.3). Parsing a template does not make
//! it trusted; it only makes it *legible*.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use saphyr::{LoadableYamlNode, MarkedYaml, Scalar, YamlData};
use saphyr_parser::{Event, Parser};

use crate::capabilities::Effort;

/// The six templates every install ships with (SUM-08).
///
/// Compiled in, so a user who deletes the directory still has working
/// templates and `fotwd templates install` can restore it from the binary.
pub const BUILTIN_SLUGS: [&str; 6] = [
    "customer-call",
    "design-review",
    "general",
    "interview",
    "one-on-one",
    "standup",
];

/// The slug used when nothing else matches.
pub const FALLBACK_SLUG: &str = "general";

const BUILTIN_SOURCES: [(&str, &str); 6] = [
    (
        "customer-call",
        include_str!("../templates/customer-call.md"),
    ),
    (
        "design-review",
        include_str!("../templates/design-review.md"),
    ),
    ("general", include_str!("../templates/general.md")),
    ("interview", include_str!("../templates/interview.md")),
    ("one-on-one", include_str!("../templates/one-on-one.md")),
    ("standup", include_str!("../templates/standup.md")),
];

/// Every key the frontmatter accepts.
const KNOWN_KEYS: [&str; 7] = [
    "name",
    "description",
    "sections",
    "extraction",
    "model_hint",
    "effort_hint",
    "default_for",
];

/// Keys inside a `sections:` entry.
const SECTION_KEYS: [&str; 3] = ["heading", "guidance", "required"];

/// Keys inside the `extraction:` map (spec 8.5's four item kinds).
const EXTRACTION_KEYS: [&str; 4] = ["action_items", "decisions", "open_questions", "follow_ups"];

/// Sampling knobs that look like they belong in a template and are rejected by
/// the API (spec 8.2: all four return 400 on Opus 5).
///
/// They are listed as *suggestion candidates* as well as rejected keys, so a
/// user who typed `temperture` is told the word they meant before being told
/// that word is not settable. Two clear errors in sequence beat one confusing
/// one.
const FORBIDDEN_KEYS: [&str; 4] = ["temperature", "top_p", "top_k", "budget_tokens"];

/// One section the template asks the model to produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// The heading text, rendered verbatim into the output shape.
    pub heading: String,
    /// What belongs under it. May be empty.
    pub guidance: String,
    /// Whether the section must appear even when the meeting said nothing
    /// about it. Defaults to `false`, because spec 8.3 clause 5 forbids
    /// inventing content and a required-but-unmentioned section is an
    /// invitation to do exactly that.
    pub required: bool,
}

/// Which of spec 8.5's extraction kinds this template wants.
///
/// All four default to on: the extraction pass is the cheap call and its
/// output is what feeds the action-item list, so opting out is a deliberate
/// choice rather than a default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractionToggles {
    /// Extract action items.
    pub action_items: bool,
    /// Extract decisions.
    pub decisions: bool,
    /// Extract open questions.
    pub open_questions: bool,
    /// Extract follow-ups.
    pub follow_ups: bool,
}

impl Default for ExtractionToggles {
    fn default() -> Self {
        Self {
            action_items: true,
            decisions: true,
            open_questions: true,
            follow_ups: true,
        }
    }
}

impl ExtractionToggles {
    /// True when the template asked for nothing at all, in which case Call B
    /// has no work to do.
    #[must_use]
    pub fn all_off(&self) -> bool {
        !self.action_items && !self.decisions && !self.open_questions && !self.follow_ups
    }
}

/// A parsed template file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    /// Filename stem — the stable identity a meeting refers to.
    pub slug: String,
    /// Human-readable name.
    pub name: String,
    /// One-line description for the picker.
    pub description: String,
    /// The output shape.
    pub sections: Vec<Section>,
    /// Which extraction kinds to run.
    pub extraction: ExtractionToggles,
    /// Model the user prefers for this template, if any. A *hint*: the preset
    /// still decides, because a template must not be able to redirect spend.
    pub model_hint: Option<String>,
    /// Effort the user prefers, if any.
    pub effort_hint: Option<Effort>,
    /// Calendar-event-title patterns this template is the default for.
    ///
    /// The divergence from Granola worth shipping (issue #36): Granola cannot
    /// set a default template at all and lists it as an open feature request.
    /// `*` matches any run of characters; matching is case-insensitive.
    pub default_for: Vec<String>,
    /// The Markdown body, verbatim, trailing whitespace trimmed.
    pub body: String,
}

/// What went wrong, in the words the user needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateErrorKind {
    /// The file does not start with a `---` frontmatter fence.
    MissingFrontmatter,
    /// The opening fence is never closed.
    UnterminatedFrontmatter,
    /// The YAML itself does not parse.
    Yaml(String),
    /// Frontmatter that is not a mapping (a list, a bare scalar, empty).
    NotAMapping,
    /// A key nobody recognises.
    UnknownKey {
        /// What was written.
        found: String,
        /// The closest known key, when one is close enough to suggest.
        suggestion: Option<String>,
    },
    /// A key that exists in the API's vocabulary but must never be sent.
    ForbiddenKey {
        /// Which one.
        key: String,
    },
    /// A key whose value is the wrong shape.
    WrongType {
        /// Which key.
        key: String,
        /// What the parser wanted.
        expected: &'static str,
        /// What it got.
        found: String,
    },
    /// A required key is absent.
    MissingKey {
        /// Which one.
        key: &'static str,
    },
    /// A value outside its allowed set.
    BadValue {
        /// Which key.
        key: String,
        /// What was written.
        found: String,
        /// What is allowed.
        allowed: &'static str,
    },
    /// The same key twice in one mapping. YAML's own answer is "last wins",
    /// which silently discards the first — exactly the class of quiet loss this
    /// module refuses.
    DuplicateKey {
        /// Which one.
        key: String,
    },
}

impl fmt::Display for TemplateErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingFrontmatter => write!(
                f,
                "a template must begin with a `---` line opening its YAML frontmatter"
            ),
            Self::UnterminatedFrontmatter => write!(
                f,
                "the YAML frontmatter opened with `---` is never closed by a matching `---`"
            ),
            Self::Yaml(msg) => write!(f, "invalid YAML: {msg}"),
            Self::NotAMapping => write!(
                f,
                "the frontmatter must be a mapping of keys to values, e.g. `name: Standup`"
            ),
            Self::UnknownKey { found, suggestion } => match suggestion {
                Some(s) => write!(f, "unknown key `{found}`, did you mean `{s}`?"),
                None => write!(
                    f,
                    "unknown key `{found}`; known keys are {}",
                    quoted_list(&KNOWN_KEYS)
                ),
            },
            Self::ForbiddenKey { key } => write!(
                f,
                "`{key}` cannot be set from a template: Claude Opus 5 returns HTTP 400 for it \
                 (docs/REQUIREMENTS.md 8.2). Use `effort_hint` instead"
            ),
            Self::WrongType {
                key,
                expected,
                found,
            } => write!(f, "`{key}` must be {expected}, found {found}"),
            Self::MissingKey { key } => write!(f, "missing required key `{key}`"),
            Self::BadValue {
                key,
                found,
                allowed,
            } => write!(f, "`{key}` must be one of {allowed}, found `{found}`"),
            Self::DuplicateKey { key } => write!(
                f,
                "duplicate key `{key}` — YAML would silently keep only the last one"
            ),
        }
    }
}

/// A parse failure, located.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateError {
    /// The file, when it came from one.
    pub path: Option<PathBuf>,
    /// 1-based line **in the file**, frontmatter fence included in the count.
    pub line: usize,
    /// 1-based column.
    pub column: usize,
    /// What went wrong.
    pub kind: TemplateErrorKind,
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(path) = &self.path {
            write!(f, "{} ", path.display())?;
        }
        write!(f, "line {}: {}", self.line, self.kind)
    }
}

impl std::error::Error for TemplateError {}

impl TemplateError {
    /// Re-label an error with the file it came from.
    #[must_use]
    pub fn with_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }

    fn at(line: usize, column: usize, kind: TemplateErrorKind) -> Self {
        Self {
            path: None,
            line,
            column,
            kind,
        }
    }
}

impl Template {
    /// Parse a template from the contents of a file.
    ///
    /// `slug` is the filename stem; it is the identity, not the `name`, so
    /// renaming a template in its own frontmatter does not orphan the meetings
    /// that used it.
    ///
    /// # Errors
    ///
    /// [`TemplateError`], always located to a line. Never a default.
    pub fn parse(slug: &str, source: &str) -> Result<Self, TemplateError> {
        let (front, front_line_offset, body) = split_frontmatter(source)?;

        // saphyr counts lines from the start of what it was handed; the
        // frontmatter starts below the opening fence, so every span it reports
        // is short by however many lines that fence occupied.
        let docs = MarkedYaml::load_from_str(front).map_err(|e| {
            TemplateError::at(
                e.marker().line() + front_line_offset,
                e.marker().col() + 1,
                TemplateErrorKind::Yaml(e.info().to_owned()),
            )
        })?;

        // Before walking the tree: the tree cannot answer this question,
        // because the loader has already collapsed a repeated key into one
        // entry holding the last value.
        check_duplicate_keys(front, front_line_offset)?;

        let doc = docs.first().ok_or_else(|| {
            TemplateError::at(front_line_offset + 1, 1, TemplateErrorKind::NotAMapping)
        })?;
        let map = as_mapping(doc, front_line_offset)?;

        let mut name: Option<String> = None;
        let mut description = String::new();
        let mut sections = Vec::new();
        let mut extraction = ExtractionToggles::default();
        let mut model_hint = None;
        let mut effort_hint = None;
        let mut default_for = Vec::new();

        // No duplicate-key bookkeeping in this loop: `check_duplicate_keys`
        // above has already ruled it out for every mapping in the document,
        // and a second check here could never fire.
        for (k, v) in map {
            let (key, line, column) = key_of(k, front_line_offset)?;
            match key.as_str() {
                "name" => name = Some(string_at(&key, v, front_line_offset)?),
                "description" => description = string_at(&key, v, front_line_offset)?,
                "model_hint" => model_hint = Some(string_at(&key, v, front_line_offset)?),
                "effort_hint" => {
                    let raw = string_at(&key, v, front_line_offset)?;
                    effort_hint = Some(parse_effort(&raw, v, front_line_offset)?);
                }
                "sections" => sections = parse_sections(v, front_line_offset)?,
                "extraction" => extraction = parse_extraction(v, front_line_offset)?,
                "default_for" => default_for = parse_string_list(&key, v, front_line_offset)?,
                _ => return Err(unknown_key(&key, line, column)),
            }
        }

        let name = name.ok_or_else(|| {
            TemplateError::at(
                front_line_offset + 1,
                1,
                TemplateErrorKind::MissingKey { key: "name" },
            )
        })?;

        Ok(Self {
            slug: slug.to_owned(),
            name,
            description,
            sections,
            extraction,
            model_hint,
            effort_hint,
            default_for,
            body: body.trim().to_owned(),
        })
    }

    /// Read and parse a template file, taking the slug from its stem.
    ///
    /// # Errors
    ///
    /// [`TemplateError`] labelled with `path`, including for an unreadable
    /// file — a template the user cannot read is still a template that did not
    /// apply, and must not be skipped quietly.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, TemplateError> {
        let path = path.as_ref();
        let slug = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let source = std::fs::read_to_string(path).map_err(|e| {
            TemplateError::at(1, 1, TemplateErrorKind::Yaml(format!("cannot read: {e}")))
                .with_path(path)
        })?;
        Self::parse(&slug, &source).map_err(|e| e.with_path(path))
    }

    /// The text handed to [`crate::prompt::assemble`] as the template body.
    ///
    /// Sections are rendered *below* the Markdown body so a template that says
    /// everything it wants in prose does not have a machine-generated list
    /// bolted on top of it, and so the last thing the model reads inside the
    /// quarantine is the concrete output shape.
    ///
    /// Nothing here is trusted. `assemble` neutralizes any delimiter this text
    /// contains, which is what stops a `heading:` of `</template>` from ending
    /// the quarantine early.
    #[must_use]
    pub fn prompt_body(&self) -> String {
        let mut out = String::new();
        if !self.body.is_empty() {
            out.push_str(&self.body);
        }
        if !self.sections.is_empty() {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str("Structure the document with these sections, in this order:\n");
            for s in &self.sections {
                out.push_str("\n- ");
                out.push_str(&s.heading);
                if s.required {
                    out.push_str(" (always include this section)");
                } else {
                    out.push_str(" (omit if the meeting did not cover it)");
                }
                if !s.guidance.is_empty() {
                    out.push_str(" — ");
                    out.push_str(&s.guidance);
                }
            }
        }
        out
    }

    /// True when this template claims `title` through one of its
    /// [`Template::default_for`] patterns.
    #[must_use]
    pub fn matches_event_title(&self, title: &str) -> bool {
        let lowered = title.to_lowercase();
        self.default_for
            .iter()
            .any(|p| glob_match(&p.to_lowercase(), &lowered))
    }

    /// How specifically this template claims `title` — the number of literal
    /// (non-`*`) characters in the matching pattern.
    ///
    /// Used to break ties: `Weekly design review` should beat `*review*`.
    fn match_specificity(&self, title: &str) -> Option<usize> {
        let lowered = title.to_lowercase();
        self.default_for
            .iter()
            .filter(|p| glob_match(&p.to_lowercase(), &lowered))
            .map(|p| p.chars().filter(|c| *c != '*').count())
            .max()
    }
}

/// Every template on disk, keyed by slug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateSet {
    templates: BTreeMap<String, Template>,
}

impl TemplateSet {
    /// The six compiled-in defaults.
    ///
    /// # Panics
    ///
    /// If a shipped template stops parsing. That is a build-time mistake and
    /// `builtins_all_parse` catches it in CI before it can reach anyone.
    #[must_use]
    pub fn builtin() -> Self {
        let mut templates = BTreeMap::new();
        for (slug, src) in BUILTIN_SOURCES {
            let t = Template::parse(slug, src)
                .unwrap_or_else(|e| panic!("shipped template {slug}.md is malformed: {e}"));
            templates.insert(slug.to_owned(), t);
        }
        Self { templates }
    }

    /// Load every `*.md` in `dir`.
    ///
    /// A missing directory yields an **empty** set rather than the builtins:
    /// "there are no template files" and "the template files say this" are
    /// different facts, and conflating them is how a caller ends up unable to
    /// tell whether installation is needed. Use [`TemplateSet::or_builtin`] to
    /// fall back explicitly.
    ///
    /// # Errors
    ///
    /// The first file that fails to parse fails the whole load, path attached.
    /// Skipping it would leave the user with a picker that silently lacks the
    /// template they just wrote.
    pub fn load(dir: impl AsRef<Path>) -> Result<Self, TemplateError> {
        let dir = dir.as_ref();
        let mut templates = BTreeMap::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(Self { templates });
        };
        // Sorted, so a directory with two broken files reports the same one
        // every time. Readdir order is filesystem-dependent and would make the
        // error message look nondeterministic.
        let mut paths: Vec<PathBuf> = entries
            .filter_map(std::result::Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .collect();
        paths.sort();

        for path in paths {
            let t = Template::from_file(&path)?;
            templates.insert(t.slug.clone(), t);
        }
        Ok(Self { templates })
    }

    /// Load `dir`, falling back to the builtins when it holds no templates.
    ///
    /// # Errors
    ///
    /// Propagates a parse failure — a broken file is never replaced by a
    /// builtin.
    pub fn load_or_builtin(dir: impl AsRef<Path>) -> Result<Self, TemplateError> {
        let set = Self::load(dir)?;
        Ok(set.or_builtin())
    }

    /// This set, or the builtins if it is empty.
    #[must_use]
    pub fn or_builtin(self) -> Self {
        if self.templates.is_empty() {
            Self::builtin()
        } else {
            self
        }
    }

    /// Look one up.
    #[must_use]
    pub fn get(&self, slug: &str) -> Option<&Template> {
        self.templates.get(slug)
    }

    /// Every template, slug order.
    pub fn iter(&self) -> impl Iterator<Item = &Template> {
        self.templates.values()
    }

    /// How many.
    #[must_use]
    pub fn len(&self) -> usize {
        self.templates.len()
    }

    /// True when nothing was loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.templates.is_empty()
    }

    /// The template a calendar event with this title should use.
    ///
    /// Most specific pattern wins; ties go to the alphabetically first slug so
    /// the answer never depends on directory order. Falls back to `general`,
    /// then to whatever is first, then to `None` for an empty set.
    #[must_use]
    pub fn for_event_title(&self, title: &str) -> Option<&Template> {
        let best = self
            .templates
            .values()
            .filter_map(|t| t.match_specificity(title).map(|s| (s, t)))
            .max_by_key(|(s, t)| (*s, std::cmp::Reverse(t.slug.clone())));
        match best {
            Some((_, t)) => Some(t),
            None => self
                .get(FALLBACK_SLUG)
                .or_else(|| self.templates.values().next()),
        }
    }

    /// Write any missing builtin into `dir` and return what was written.
    ///
    /// Never overwrites: a user who edited `standup.md` has made it *their*
    /// file, and restoring our version over it would be the same silent data
    /// loss this module exists to prevent. Deleting the file is the way to ask
    /// for ours back.
    ///
    /// # Errors
    ///
    /// Filesystem failures, with the path attached by the caller.
    pub fn install_builtins(dir: impl AsRef<Path>) -> std::io::Result<Vec<PathBuf>> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)?;
        let mut written = Vec::new();
        for (slug, src) in BUILTIN_SOURCES {
            let path = dir.join(format!("{slug}.md"));
            if path.exists() {
                continue;
            }
            std::fs::write(&path, src)?;
            written.push(path);
        }
        Ok(written)
    }
}

/// Where templates live: `~/.flyonthewall/templates`, overridable with
/// `FOTW_TEMPLATES_DIR`.
///
/// A dotfile directory in `$HOME` rather than the app data root from §9.2,
/// because issue #36's stated advantage is that a user can `git init` this
/// directory and share it — and `~/Library/Application Support/...` is neither
/// discoverable in a shell nor a place anyone expects to keep a repository.
#[must_use]
pub fn default_templates_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("FOTW_TEMPLATES_DIR") {
        return PathBuf::from(dir);
    }
    std::env::var("HOME").map_or_else(
        |_| PathBuf::from(".flyonthewall/templates"),
        |h| PathBuf::from(h).join(".flyonthewall/templates"),
    )
}

// ------------------------------------------------------------------ internals

/// Split `---\n<yaml>\n---\n<body>`.
///
/// Returns the frontmatter text, how many lines precede it (so spans can be
/// translated back to file lines), and the body.
fn split_frontmatter(source: &str) -> Result<(&str, usize, &str), TemplateError> {
    // A UTF-8 BOM in front of the fence is common from Windows editors and is
    // invisible in every one of them, so it gets its own handling rather than
    // becoming "missing frontmatter".
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);

    let after_open = if let Some(rest) = source.strip_prefix("---\n") {
        rest
    } else if let Some(rest) = source.strip_prefix("---\r\n") {
        rest
    } else {
        return Err(TemplateError::at(
            1,
            1,
            TemplateErrorKind::MissingFrontmatter,
        ));
    };

    let mut offset = 0usize;
    for line in after_open.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == "---" || trimmed == "..." {
            let front = &after_open[..offset];
            let body = &after_open[offset + line.len()..];
            return Ok((front, 1, body));
        }
        offset += line.len();
    }
    Err(TemplateError::at(
        1,
        1,
        TemplateErrorKind::UnterminatedFrontmatter,
    ))
}

/// Reject a mapping that names the same key twice, anywhere in the document.
///
/// This runs on the **event stream** rather than on the loaded tree, and it has
/// to. `saphyr`'s loader inserts each pair into a `LinkedHashMap`, so by the
/// time a document exists, `name:` written twice is one entry holding the
/// second value — YAML's specified "last wins", applied silently. A user who
/// pastes a second `sections:` block below the first would get only the second
/// one and no indication that the first was discarded, which is the same class
/// of quiet loss as ignoring the whole file.
///
/// Nesting is tracked so that `heading:` appearing once inside each of five
/// section entries is not mistaken for five duplicates.
fn check_duplicate_keys(front: &str, off: usize) -> Result<(), TemplateError> {
    /// What kind of container the parser is currently inside, and -- for a
    /// mapping -- whether the next complete node is a key or a value.
    enum Frame {
        Mapping {
            seen: Vec<String>,
            expecting_key: bool,
        },
        Sequence,
    }

    /// Record that one complete node has just been read in the enclosing
    /// container, flipping a mapping between key and value position. A nested
    /// sequence or mapping counts as exactly one node, which is why this is
    /// called on `MappingEnd`/`SequenceEnd` and not on their contents.
    fn advance(stack: &mut [Frame]) {
        if let Some(Frame::Mapping { expecting_key, .. }) = stack.last_mut() {
            *expecting_key = !*expecting_key;
        }
    }

    fn at_key(stack: &[Frame]) -> bool {
        matches!(
            stack.last(),
            Some(Frame::Mapping {
                expecting_key: true,
                ..
            })
        )
    }

    let mut stack: Vec<Frame> = Vec::new();
    let mut parser = Parser::new_from_str(front);

    while let Some(next) = parser.next_event() {
        // A parse error here is reported by the loader with a better message,
        // so stopping quietly leaves that error as the one the user sees.
        let Ok((event, span)) = next else {
            return Ok(());
        };
        match event {
            Event::Scalar(value, _, _, _) => {
                if at_key(&stack) {
                    let key = value.to_string();
                    if let Some(Frame::Mapping { seen, .. }) = stack.last_mut() {
                        if seen.contains(&key) {
                            return Err(TemplateError::at(
                                span.start.line() + off,
                                span.start.col() + 1,
                                TemplateErrorKind::DuplicateKey { key },
                            ));
                        }
                        seen.push(key);
                    }
                }
                advance(&mut stack);
            }
            Event::Alias(_) => advance(&mut stack),
            Event::MappingStart(..) => stack.push(Frame::Mapping {
                seen: Vec::new(),
                expecting_key: true,
            }),
            Event::SequenceStart(..) => stack.push(Frame::Sequence),
            Event::MappingEnd | Event::SequenceEnd => {
                stack.pop();
                advance(&mut stack);
            }
            _ => {}
        }
    }
    Ok(())
}

fn as_mapping<'a, 'i>(
    node: &'a MarkedYaml<'i>,
    off: usize,
) -> Result<impl Iterator<Item = (&'a MarkedYaml<'i>, &'a MarkedYaml<'i>)>, TemplateError> {
    match &node.data {
        YamlData::Mapping(m) => Ok(m.iter()),
        _ => Err(TemplateError::at(
            node.span.start.line() + off,
            node.span.start.col() + 1,
            TemplateErrorKind::NotAMapping,
        )),
    }
}

fn key_of(node: &MarkedYaml<'_>, off: usize) -> Result<(String, usize, usize), TemplateError> {
    let line = node.span.start.line() + off;
    let column = node.span.start.col() + 1;
    match scalar_text(node) {
        Some(s) => Ok((s, line, column)),
        None => Err(TemplateError::at(
            line,
            column,
            TemplateErrorKind::WrongType {
                key: "<key>".to_owned(),
                expected: "a plain name",
                found: describe(node),
            },
        )),
    }
}

fn unknown_key(key: &str, line: usize, column: usize) -> TemplateError {
    if FORBIDDEN_KEYS.contains(&key) {
        return TemplateError::at(
            line,
            column,
            TemplateErrorKind::ForbiddenKey {
                key: key.to_owned(),
            },
        );
    }
    TemplateError::at(
        line,
        column,
        TemplateErrorKind::UnknownKey {
            found: key.to_owned(),
            suggestion: suggest(key, KNOWN_KEYS.iter().chain(FORBIDDEN_KEYS.iter()).copied()),
        },
    )
}

/// The nearest candidate within an edit distance that scales with the word's
/// length, or `None`.
///
/// A fixed threshold is wrong at both ends: distance 2 on a four-letter key is
/// half the word (`name` -> `time` is not a typo), and distance 2 on
/// `open_questions` is a single slip.
fn suggest<'a>(found: &str, candidates: impl Iterator<Item = &'a str>) -> Option<String> {
    let budget = match found.chars().count() {
        0..=4 => 1,
        5..=8 => 2,
        _ => 3,
    };
    candidates
        .map(|c| (levenshtein(found, c), c))
        .filter(|(d, _)| *d <= budget)
        .min_by_key(|(d, c)| (*d, c.len()))
        .map(|(_, c)| c.to_owned())
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

fn scalar_text(node: &MarkedYaml<'_>) -> Option<String> {
    match &node.data {
        YamlData::Value(Scalar::String(s)) => Some(s.to_string()),
        YamlData::Value(Scalar::Integer(i)) => Some(i.to_string()),
        YamlData::Value(Scalar::Boolean(b)) => Some(b.to_string()),
        YamlData::Representation(raw, _, _) => Some(raw.to_string()),
        _ => None,
    }
}

/// What a node is, in the words an error message can use.
fn describe(node: &MarkedYaml<'_>) -> String {
    match &node.data {
        YamlData::Value(Scalar::Null) | YamlData::BadValue => "nothing".to_owned(),
        YamlData::Value(Scalar::Boolean(b)) => format!("the boolean `{b}`"),
        YamlData::Value(Scalar::Integer(i)) => format!("the number `{i}`"),
        YamlData::Value(Scalar::FloatingPoint(x)) => format!("the number `{}`", x.into_inner()),
        YamlData::Value(Scalar::String(s)) => format!("the text `{s}`"),
        YamlData::Representation(raw, _, _) => format!("`{raw}`"),
        YamlData::Sequence(_) => "a list".to_owned(),
        YamlData::Mapping(_) => "a mapping".to_owned(),
        YamlData::Alias(_) | YamlData::Tagged(_, _) => "an unsupported YAML node".to_owned(),
    }
}

fn string_at(key: &str, node: &MarkedYaml<'_>, off: usize) -> Result<String, TemplateError> {
    // A YAML string is a string. An integer where a string was wanted is a
    // typo often enough that coercing it would hide a mistake, so it is an
    // error with the line on it.
    match &node.data {
        YamlData::Value(Scalar::String(s)) => Ok(s.to_string()),
        YamlData::Representation(raw, _, _) => Ok(raw.to_string()),
        _ => Err(TemplateError::at(
            node.span.start.line() + off,
            node.span.start.col() + 1,
            TemplateErrorKind::WrongType {
                key: key.to_owned(),
                expected: "text",
                found: describe(node),
            },
        )),
    }
}

fn bool_at(key: &str, node: &MarkedYaml<'_>, off: usize) -> Result<bool, TemplateError> {
    match &node.data {
        YamlData::Value(Scalar::Boolean(b)) => Ok(*b),
        _ => Err(TemplateError::at(
            node.span.start.line() + off,
            node.span.start.col() + 1,
            TemplateErrorKind::WrongType {
                key: key.to_owned(),
                expected: "true or false",
                found: describe(node),
            },
        )),
    }
}

fn parse_effort(raw: &str, node: &MarkedYaml<'_>, off: usize) -> Result<Effort, TemplateError> {
    match raw {
        "low" => Ok(Effort::Low),
        "medium" => Ok(Effort::Medium),
        "high" => Ok(Effort::High),
        _ => Err(TemplateError::at(
            node.span.start.line() + off,
            node.span.start.col() + 1,
            TemplateErrorKind::BadValue {
                key: "effort_hint".to_owned(),
                found: raw.to_owned(),
                allowed: "`low`, `medium`, `high`",
            },
        )),
    }
}

fn parse_string_list(
    key: &str,
    node: &MarkedYaml<'_>,
    off: usize,
) -> Result<Vec<String>, TemplateError> {
    let YamlData::Sequence(items) = &node.data else {
        return Err(TemplateError::at(
            node.span.start.line() + off,
            node.span.start.col() + 1,
            TemplateErrorKind::WrongType {
                key: key.to_owned(),
                expected: "a list",
                found: describe(node),
            },
        ));
    };
    items.iter().map(|i| string_at(key, i, off)).collect()
}

fn parse_sections(node: &MarkedYaml<'_>, off: usize) -> Result<Vec<Section>, TemplateError> {
    let YamlData::Sequence(items) = &node.data else {
        return Err(TemplateError::at(
            node.span.start.line() + off,
            node.span.start.col() + 1,
            TemplateErrorKind::WrongType {
                key: "sections".to_owned(),
                expected: "a list of `{heading, guidance, required}` entries",
                found: describe(node),
            },
        ));
    };

    let mut out = Vec::new();
    for item in items {
        let map = as_mapping(item, off)?;
        let mut heading = None;
        let mut guidance = String::new();
        let mut required = false;

        for (k, v) in map {
            let (key, line, column) = key_of(k, off)?;
            match key.as_str() {
                "heading" => heading = Some(string_at(&key, v, off)?),
                "guidance" => guidance = string_at(&key, v, off)?,
                "required" => required = bool_at(&key, v, off)?,
                _ => {
                    return Err(TemplateError::at(
                        line,
                        column,
                        TemplateErrorKind::UnknownKey {
                            found: key.clone(),
                            suggestion: suggest(&key, SECTION_KEYS.iter().copied()),
                        },
                    ));
                }
            }
        }

        let heading = heading.ok_or_else(|| {
            TemplateError::at(
                item.span.start.line() + off,
                item.span.start.col() + 1,
                TemplateErrorKind::MissingKey { key: "heading" },
            )
        })?;
        out.push(Section {
            heading,
            guidance,
            required,
        });
    }
    Ok(out)
}

fn parse_extraction(node: &MarkedYaml<'_>, off: usize) -> Result<ExtractionToggles, TemplateError> {
    let map = as_mapping(node, off)?;
    let mut t = ExtractionToggles::default();
    for (k, v) in map {
        let (key, line, column) = key_of(k, off)?;
        let value = bool_at(&key, v, off)?;
        match key.as_str() {
            "action_items" => t.action_items = value,
            "decisions" => t.decisions = value,
            "open_questions" => t.open_questions = value,
            "follow_ups" => t.follow_ups = value,
            _ => {
                return Err(TemplateError::at(
                    line,
                    column,
                    TemplateErrorKind::UnknownKey {
                        found: key.clone(),
                        suggestion: suggest(&key, EXTRACTION_KEYS.iter().copied()),
                    },
                ));
            }
        }
    }
    Ok(t)
}

/// `*` matches any run of characters, including none. Everything else is
/// literal. Deliberately not a regex: a template file is not a place to accept
/// a language with catastrophic backtracking in it.
fn glob_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let mut rest = text;
    // The first literal must sit at the very start unless the pattern opened
    // with `*`.
    if let Some(first) = parts.first()
        && !first.is_empty()
    {
        match rest.strip_prefix(first) {
            Some(r) => rest = r,
            None => return false,
        }
    }
    // ...and the last must sit at the very end unless it closed with one.
    let last = parts[parts.len() - 1];
    if !last.is_empty() {
        if rest.len() < last.len() || !rest.ends_with(last) {
            return false;
        }
        rest = &rest[..rest.len() - last.len()];
    }
    for middle in &parts[1..parts.len() - 1] {
        if middle.is_empty() {
            continue;
        }
        match rest.find(middle) {
            Some(at) => rest = &rest[at + middle.len()..],
            None => return false,
        }
    }
    true
}

fn quoted_list(keys: &[&str]) -> String {
    keys.iter()
        .map(|k| format!("`{k}`"))
        .collect::<Vec<_>>()
        .join(", ")
}
