use std::path::PathBuf;

/// A coding CLI that owns a session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Provider {
    /// OpenAI Codex CLI.
    Codex,
    /// OpenCode CLI.
    OpenCode,
    /// Anthropic Claude Code CLI.
    Claude,
}

impl Provider {
    /// Returns the short provider label displayed by the finder.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::Claude => "claude",
        }
    }
}

/// Provider-independent token counters for a session.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TokenUsage {
    /// Non-cached input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Tokens written to a provider cache.
    pub cache_creation_tokens: u64,
    /// Tokens read from a provider cache.
    pub cache_read_tokens: u64,
}

impl TokenUsage {
    /// Returns input, output, cache creation, and cache read tokens combined.
    #[must_use]
    pub fn total_tokens(self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_creation_tokens)
            .saturating_add(self.cache_read_tokens)
    }

    pub(crate) fn add_assign(&mut self, other: Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .saturating_add(other.cache_creation_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
    }

    pub(crate) fn saturating_sub(self, previous: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_sub(previous.input_tokens),
            output_tokens: self.output_tokens.saturating_sub(previous.output_tokens),
            cache_creation_tokens: self
                .cache_creation_tokens
                .saturating_sub(previous.cache_creation_tokens),
            cache_read_tokens: self
                .cache_read_tokens
                .saturating_sub(previous.cache_read_tokens),
        }
    }

    pub(crate) fn has_decreased_from(self, previous: Self) -> bool {
        self.input_tokens < previous.input_tokens
            || self.output_tokens < previous.output_tokens
            || self.cache_creation_tokens < previous.cache_creation_tokens
            || self.cache_read_tokens < previous.cache_read_tokens
    }
}

pub(crate) fn add_usage(total: &mut Option<TokenUsage>, usage: TokenUsage) {
    match total {
        Some(total) => total.add_assign(usage),
        None => *total = Some(usage),
    }
}

/// Provider-independent session data used by the finder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Session {
    /// CLI that owns the session.
    pub provider: Provider,
    /// Provider session identifier used to resume it.
    pub id: String,
    /// Provider-generated title or summary, when available.
    pub title: Option<String>,
    /// Most recently recorded model identifier, when available.
    pub model: Option<String>,
    /// Most recently recorded reasoning-effort setting, when available.
    pub reasoning_effort: Option<String>,
    /// Working directory associated with the session.
    pub directory: Option<PathBuf>,
    /// Last activity time as Unix milliseconds.
    pub updated_at: i64,
    /// First user-authored message.
    pub first_user_message: String,
    /// Most recent user-authored message.
    pub last_user_message: String,
    /// Most recent assistant text response.
    pub last_assistant_message: Option<String>,
    /// All user-authored messages; this is the only message content searched.
    pub user_messages: Vec<String>,
    /// Token counters recorded by the provider, when available.
    pub usage: Option<TokenUsage>,
}

impl Session {
    /// Returns searchable text containing the directory, title, and user-authored messages.
    #[must_use]
    pub fn search_text(&self) -> String {
        let capacity = self.title.as_ref().map_or(0, String::len)
            + self
                .directory
                .as_ref()
                .map_or(0, |path| path.as_os_str().len())
            + self.user_messages.iter().map(String::len).sum::<usize>()
            + self.user_messages.len()
            + 2;
        let mut text = String::with_capacity(capacity);
        if let Some(title) = &self.title {
            text.push_str(title);
            text.push('\n');
        }
        if let Some(directory) = &self.directory {
            text.push_str(&directory.to_string_lossy());
            text.push('\n');
        }
        for message in &self.user_messages {
            text.push_str(message);
            text.push('\n');
        }
        text
    }
}

pub(crate) struct SessionHeader {
    pub(crate) provider: Provider,
    pub(crate) id: String,
    pub(crate) title: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) directory: Option<PathBuf>,
    pub(crate) updated_at: i64,
}

pub(crate) fn finish_session(
    header: SessionHeader,
    user_messages: Vec<String>,
    last_assistant_message: Option<String>,
    usage: Option<TokenUsage>,
) -> Option<Session> {
    Some(Session {
        provider: header.provider,
        id: header.id,
        title: header.title.and_then(|value| normalized_text(&value)),
        model: header.model,
        reasoning_effort: header.reasoning_effort,
        directory: header.directory,
        updated_at: header.updated_at,
        first_user_message: user_messages.first()?.clone(),
        last_user_message: user_messages.last()?.clone(),
        last_assistant_message,
        user_messages,
        usage,
    })
}

pub(crate) fn normalized_text(value: &str) -> Option<String> {
    let mut words = value.split_whitespace();
    let first = words.next()?;
    let mut text = String::with_capacity(value.len());
    text.push_str(first);
    for word in words {
        text.push(' ');
        text.push_str(word);
    }
    Some(text)
}

pub(crate) fn timestamp_millis(value: &serde_json::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|number| i64::try_from(number).ok()))
        .or_else(|| {
            value
                .as_str()
                .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
                .map(|time| time.timestamp_millis())
        })
}

pub(crate) fn text_blocks(content: &serde_json::Value, accepted_types: &[&str]) -> Option<String> {
    if let Some(text) = content.as_str() {
        return normalized_text(text);
    }
    let mut text = String::new();
    for block in content.as_array()?.iter().filter(|block| {
        block
            .get("type")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|kind| accepted_types.contains(&kind))
    }) {
        let Some(block_text) = block.get("text").and_then(serde_json::Value::as_str) else {
            continue;
        };
        for word in block_text.split_whitespace() {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(word);
        }
    }
    (!text.is_empty()).then_some(text)
}
