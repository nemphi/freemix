//! Provider-neutral moderated social and chat aggregation.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::error::Error;
use std::fmt;

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            #[must_use]
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::new(value)
            }
        }
    };
}

string_id!(ProviderId);
string_id!(AccountId);
string_id!(MessageId);
string_id!(AuthorId);

/// Milliseconds since the Unix epoch.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Timestamp(pub u64);

/// The provider-scoped identity used for duplicate suppression.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MessageKey {
    pub provider_id: ProviderId,
    pub account_id: AccountId,
    pub message_id: MessageId,
}

impl MessageKey {
    #[must_use]
    pub fn new(
        provider_id: impl Into<ProviderId>,
        account_id: impl Into<AccountId>,
        message_id: impl Into<MessageId>,
    ) -> Self {
        Self {
            provider_id: provider_id.into(),
            account_id: account_id.into(),
            message_id: message_id.into(),
        }
    }
}

/// An untrusted message received from a provider adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IncomingMessage {
    pub provider_id: ProviderId,
    pub account_id: AccountId,
    pub message_id: MessageId,
    pub author_id: AuthorId,
    pub author_name: String,
    pub author_handle: Option<String>,
    pub body: String,
    pub created_at: Timestamp,
    pub expires_at: Option<Timestamp>,
}

impl IncomingMessage {
    #[must_use]
    pub fn key(&self) -> MessageKey {
        MessageKey {
            provider_id: self.provider_id.clone(),
            account_id: self.account_id.clone(),
            message_id: self.message_id.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ModerationStatus {
    Pending,
    Approved,
    Rejected,
    Blocked,
    Expired,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModerationState {
    Pending,
    Approved,
    Rejected { reason: String },
    Blocked,
    Expired,
}

impl ModerationState {
    #[must_use]
    pub const fn status(&self) -> ModerationStatus {
        match self {
            Self::Pending => ModerationStatus::Pending,
            Self::Approved => ModerationStatus::Approved,
            Self::Rejected { .. } => ModerationStatus::Rejected,
            Self::Blocked => ModerationStatus::Blocked,
            Self::Expired => ModerationStatus::Expired,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModerationFlag {
    Profanity(String),
    Redacted(String),
}

/// A message after content policy has been applied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SocialMessage {
    pub key: MessageKey,
    pub author_id: AuthorId,
    pub author_name: String,
    pub author_handle: Option<String>,
    pub body: String,
    pub created_at: Timestamp,
    pub ingested_at: Timestamp,
    pub expires_at: Option<Timestamp>,
    pub moderation: ModerationState,
    pub flags: Vec<ModerationFlag>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProfanityAction {
    Allow,
    Flag,
    Redact { replacement: String },
    Reject,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfanityPolicy {
    pub terms: BTreeSet<String>,
    pub action: ProfanityAction,
}

impl Default for ProfanityPolicy {
    fn default() -> Self {
        Self {
            terms: BTreeSet::new(),
            action: ProfanityAction::Flag,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RedactionRule {
    pub literal: String,
    pub replacement: String,
}

impl RedactionRule {
    #[must_use]
    pub fn new(literal: impl Into<String>, replacement: impl Into<String>) -> Self {
        Self {
            literal: literal.into(),
            replacement: replacement.into(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ContentPolicy {
    pub profanity: ProfanityPolicy,
    pub redactions: Vec<RedactionRule>,
}

struct PolicyResult {
    body: String,
    flags: Vec<ModerationFlag>,
    rejection: Option<String>,
}

impl ContentPolicy {
    fn apply(&self, body: &str) -> PolicyResult {
        let found_terms = profanity_terms(body, &self.profanity.terms);
        let mut flags = Vec::new();

        if matches!(self.profanity.action, ProfanityAction::Reject) && !found_terms.is_empty() {
            return PolicyResult {
                body: body.to_owned(),
                flags: found_terms
                    .iter()
                    .cloned()
                    .map(ModerationFlag::Profanity)
                    .collect(),
                rejection: Some("profanity policy".to_owned()),
            };
        }

        if !matches!(self.profanity.action, ProfanityAction::Allow) {
            flags.extend(found_terms.iter().cloned().map(ModerationFlag::Profanity));
        }

        let mut processed = match &self.profanity.action {
            ProfanityAction::Redact { replacement } => {
                redact_words(body, &self.profanity.terms, replacement)
            }
            ProfanityAction::Allow | ProfanityAction::Flag | ProfanityAction::Reject => {
                body.to_owned()
            }
        };

        for rule in &self.redactions {
            if !rule.literal.is_empty() && processed.contains(&rule.literal) {
                processed = processed.replace(&rule.literal, &rule.replacement);
                flags.push(ModerationFlag::Redacted(rule.literal.clone()));
            }
        }

        PolicyResult {
            body: processed,
            flags,
            rejection: None,
        }
    }
}

fn is_word_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn profanity_terms(body: &str, terms: &BTreeSet<String>) -> Vec<String> {
    let mut found = BTreeSet::new();
    for word in body.split(|character: char| !is_word_character(character)) {
        if let Some(term) = terms.iter().find(|term| word.eq_ignore_ascii_case(term)) {
            found.insert(term.clone());
        }
    }
    found.into_iter().collect()
}

fn redact_words(body: &str, terms: &BTreeSet<String>, replacement: &str) -> String {
    let mut output = String::with_capacity(body.len());
    let mut word_start = None;

    for (index, character) in body.char_indices() {
        if is_word_character(character) {
            word_start.get_or_insert(index);
        } else if let Some(start) = word_start.take() {
            push_redacted_word(&mut output, &body[start..index], terms, replacement);
            output.push(character);
        } else {
            output.push(character);
        }
    }

    if let Some(start) = word_start {
        push_redacted_word(&mut output, &body[start..], terms, replacement);
    }

    output
}

fn push_redacted_word(
    output: &mut String,
    word: &str,
    terms: &BTreeSet<String>,
    replacement: &str,
) {
    if terms.iter().any(|term| word.eq_ignore_ascii_case(term)) {
        output.push_str(replacement);
    } else {
        output.push_str(word);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IngestOutcome {
    Accepted(MessageKey),
    Duplicate(MessageKey),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SocialError {
    QueueFull { capacity: usize },
    MessageNotFound(MessageKey),
    MessageNotPending(MessageKey),
}

impl fmt::Display for SocialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QueueFull { capacity } => {
                write!(formatter, "moderation queue is full (capacity {capacity})")
            }
            Self::MessageNotFound(key) => write!(formatter, "message not found: {key:?}"),
            Self::MessageNotPending(key) => write!(formatter, "message is not pending: {key:?}"),
        }
    }
}

impl Error for SocialError {}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SearchOrder {
    #[default]
    OldestFirst,
    NewestFirst,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MessageFilter {
    pub query: Option<String>,
    pub providers: BTreeSet<ProviderId>,
    pub accounts: BTreeSet<AccountId>,
    pub statuses: BTreeSet<ModerationStatus>,
    pub author_id: Option<AuthorId>,
    pub created_from: Option<Timestamp>,
    pub created_through: Option<Timestamp>,
    pub order: SearchOrder,
    pub limit: Option<usize>,
}

impl MessageFilter {
    fn matches(&self, message: &SocialMessage) -> bool {
        if !self.providers.is_empty() && !self.providers.contains(&message.key.provider_id) {
            return false;
        }
        if !self.accounts.is_empty() && !self.accounts.contains(&message.key.account_id) {
            return false;
        }
        if !self.statuses.is_empty() && !self.statuses.contains(&message.moderation.status()) {
            return false;
        }
        if self
            .author_id
            .as_ref()
            .is_some_and(|author_id| author_id != &message.author_id)
        {
            return false;
        }
        if self
            .created_from
            .is_some_and(|created_from| message.created_at < created_from)
        {
            return false;
        }
        if self
            .created_through
            .is_some_and(|created_through| message.created_at > created_through)
        {
            return false;
        }
        self.query.as_ref().is_none_or(|query| {
            let query = query.to_lowercase();
            message.body.to_lowercase().contains(&query)
                || message.author_name.to_lowercase().contains(&query)
                || message
                    .author_handle
                    .as_ref()
                    .is_some_and(|handle| handle.to_lowercase().contains(&query))
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TitleSourceField {
    AuthorName,
    AuthorHandle,
    Body,
    ProviderId,
    AccountId,
    MessageId,
    CreatedAt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TitleFieldBinding {
    pub title_field: String,
    pub source: TitleSourceField,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TitleFieldMapping {
    pub bindings: Vec<TitleFieldBinding>,
}

impl TitleFieldMapping {
    #[must_use]
    pub fn map(&self, message: &SocialMessage) -> BTreeMap<String, String> {
        self.bindings
            .iter()
            .map(|binding| {
                let value = match binding.source {
                    TitleSourceField::AuthorName => message.author_name.clone(),
                    TitleSourceField::AuthorHandle => {
                        message.author_handle.clone().unwrap_or_default()
                    }
                    TitleSourceField::Body => message.body.clone(),
                    TitleSourceField::ProviderId => message.key.provider_id.to_string(),
                    TitleSourceField::AccountId => message.key.account_id.to_string(),
                    TitleSourceField::MessageId => message.key.message_id.to_string(),
                    TitleSourceField::CreatedAt => message.created_at.0.to_string(),
                };
                (binding.title_field.clone(), value)
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ProviderCapability {
    LiveMessages,
    HistoricalMessages,
    AuthorizationRefresh,
    RateLimitStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthorizationState {
    Unknown,
    NotRequired,
    Required,
    Authorizing,
    Authorized,
    Expired,
    Revoked,
    Failed(String),
}

impl AuthorizationState {
    #[must_use]
    pub const fn permits_fetch(&self) -> bool {
        matches!(self, Self::NotRequired | Self::Authorized)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RateLimitRecord {
    pub limit: Option<u32>,
    pub remaining: Option<u32>,
    pub reset_at: Option<Timestamp>,
    pub observed_at: Timestamp,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackoffRecord {
    pub attempt: u32,
    pub retry_at: Timestamp,
    pub reason: String,
    pub observed_at: Timestamp,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderRateRecord {
    pub provider_id: ProviderId,
    pub rate: RateLimitRecord,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderBackoffRecord {
    pub provider_id: ProviderId,
    pub backoff: BackoffRecord,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderState {
    pub capabilities: BTreeSet<ProviderCapability>,
    pub authorization: AuthorizationState,
    pub cursor: Option<String>,
    pub latest_rate: Option<RateLimitRecord>,
    pub latest_backoff: Option<BackoffRecord>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProviderBatch {
    pub messages: Vec<IncomingMessage>,
    pub next_cursor: Option<String>,
    pub rate: Option<RateLimitRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderError {
    Authorization(AuthorizationState),
    RateLimited(RateLimitRecord),
    Backoff(BackoffRecord),
    Transport(String),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Authorization(state) => write!(formatter, "provider authorization: {state:?}"),
            Self::RateLimited(_) => formatter.write_str("provider rate limited"),
            Self::Backoff(_) => formatter.write_str("provider is backing off"),
            Self::Transport(message) => write!(formatter, "provider transport: {message}"),
        }
    }
}

impl Error for ProviderError {}

/// Adapter contract implemented by provider integrations and test fakes.
pub trait SocialProvider {
    fn provider_id(&self) -> ProviderId;
    fn capabilities(&self) -> BTreeSet<ProviderCapability>;
    fn authorization_state(&self) -> AuthorizationState;

    /// Fetches messages after the supplied opaque provider cursor.
    ///
    /// # Errors
    ///
    /// Returns a provider-specific state record when fetching cannot proceed.
    fn fetch(
        &mut self,
        cursor: Option<&str>,
        now: Timestamp,
    ) -> Result<ProviderBatch, ProviderError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PollReport {
    pub provider_id: ProviderId,
    pub outcomes: Vec<Result<IngestOutcome, SocialError>>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct BlockedAuthor {
    provider: ProviderId,
    account: AccountId,
    author: AuthorId,
}

/// In-memory aggregation and moderation state.
pub struct SocialAggregator {
    queue_capacity: usize,
    policy: ContentPolicy,
    messages: HashMap<MessageKey, SocialMessage>,
    message_order: Vec<MessageKey>,
    pending: VecDeque<MessageKey>,
    blocked_authors: HashSet<BlockedAuthor>,
    providers: HashMap<ProviderId, ProviderState>,
    rate_records: Vec<ProviderRateRecord>,
    backoff_records: Vec<ProviderBackoffRecord>,
}

impl SocialAggregator {
    #[must_use]
    pub fn new(queue_capacity: usize, policy: ContentPolicy) -> Self {
        Self {
            queue_capacity,
            policy,
            messages: HashMap::new(),
            message_order: Vec::new(),
            pending: VecDeque::new(),
            blocked_authors: HashSet::new(),
            providers: HashMap::new(),
            rate_records: Vec::new(),
            backoff_records: Vec::new(),
        }
    }

    #[must_use]
    pub const fn queue_capacity(&self) -> usize {
        self.queue_capacity
    }

    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    #[must_use]
    pub fn message(&self, key: &MessageKey) -> Option<&SocialMessage> {
        self.messages.get(key)
    }

    #[must_use]
    pub fn pending_messages(&self) -> Vec<&SocialMessage> {
        self.pending
            .iter()
            .filter_map(|key| self.messages.get(key))
            .collect()
    }

    /// Ingests one message. A failed queue insertion is not marked as seen and can be retried.
    ///
    /// # Errors
    ///
    /// Returns [`SocialError::QueueFull`] when a pending message cannot fit in the queue.
    pub fn ingest(
        &mut self,
        incoming: IncomingMessage,
        now: Timestamp,
    ) -> Result<IngestOutcome, SocialError> {
        let key = incoming.key();
        if self.messages.contains_key(&key) {
            return Ok(IngestOutcome::Duplicate(key));
        }

        let policy_result = self.policy.apply(&incoming.body);
        let blocked = self.blocked_authors.contains(&BlockedAuthor {
            provider: incoming.provider_id.clone(),
            account: incoming.account_id.clone(),
            author: incoming.author_id.clone(),
        });
        let expired = incoming
            .expires_at
            .is_some_and(|expires_at| expires_at <= now);
        let moderation = if blocked {
            ModerationState::Blocked
        } else if expired {
            ModerationState::Expired
        } else if let Some(reason) = policy_result.rejection {
            ModerationState::Rejected { reason }
        } else {
            ModerationState::Pending
        };

        if moderation == ModerationState::Pending && self.pending.len() >= self.queue_capacity {
            return Err(SocialError::QueueFull {
                capacity: self.queue_capacity,
            });
        }

        let message = SocialMessage {
            key: key.clone(),
            author_id: incoming.author_id,
            author_name: incoming.author_name,
            author_handle: incoming.author_handle,
            body: policy_result.body,
            created_at: incoming.created_at,
            ingested_at: now,
            expires_at: incoming.expires_at,
            moderation,
            flags: policy_result.flags,
        };
        if message.moderation == ModerationState::Pending {
            self.pending.push_back(key.clone());
        }
        self.message_order.push(key.clone());
        self.messages.insert(key.clone(), message);
        Ok(IngestOutcome::Accepted(key))
    }

    /// Sorts a provider batch chronologically before ingestion.
    pub fn ingest_batch(
        &mut self,
        mut messages: Vec<IncomingMessage>,
        now: Timestamp,
    ) -> Vec<Result<IngestOutcome, SocialError>> {
        messages.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.key().cmp(&right.key()))
        });
        messages
            .into_iter()
            .map(|message| self.ingest(message, now))
            .collect()
    }

    /// Approves a pending message.
    ///
    /// # Errors
    ///
    /// Returns an error when the message does not exist or is no longer pending.
    pub fn approve(&mut self, key: &MessageKey) -> Result<(), SocialError> {
        self.transition_pending(key, ModerationState::Approved)
    }

    /// Rejects a pending message with a reason.
    ///
    /// # Errors
    ///
    /// Returns an error when the message does not exist or is no longer pending.
    pub fn reject(
        &mut self,
        key: &MessageKey,
        reason: impl Into<String>,
    ) -> Result<(), SocialError> {
        self.transition_pending(
            key,
            ModerationState::Rejected {
                reason: reason.into(),
            },
        )
    }

    /// Explicitly expires a pending message.
    ///
    /// # Errors
    ///
    /// Returns an error when the message does not exist or is no longer pending.
    pub fn expire(&mut self, key: &MessageKey) -> Result<(), SocialError> {
        self.transition_pending(key, ModerationState::Expired)
    }

    /// Blocks the message author for this provider account and blocks future messages from them.
    ///
    /// # Errors
    ///
    /// Returns an error when the message does not exist or is no longer pending.
    pub fn block(&mut self, key: &MessageKey) -> Result<(), SocialError> {
        let message = self
            .messages
            .get(key)
            .ok_or_else(|| SocialError::MessageNotFound(key.clone()))?;
        if message.moderation != ModerationState::Pending {
            return Err(SocialError::MessageNotPending(key.clone()));
        }
        self.blocked_authors.insert(BlockedAuthor {
            provider: key.provider_id.clone(),
            account: key.account_id.clone(),
            author: message.author_id.clone(),
        });
        self.transition_pending(key, ModerationState::Blocked)
    }

    /// Expires pending messages whose provider expiry has elapsed.
    pub fn expire_due(&mut self, now: Timestamp) -> usize {
        let due: Vec<_> = self
            .pending
            .iter()
            .filter(|key| {
                self.messages
                    .get(*key)
                    .and_then(|message| message.expires_at)
                    .is_some_and(|expires_at| expires_at <= now)
            })
            .cloned()
            .collect();
        for key in &due {
            let result = self.transition_pending(key, ModerationState::Expired);
            debug_assert!(
                result.is_ok(),
                "pending queue contained non-pending message"
            );
        }
        due.len()
    }

    fn transition_pending(
        &mut self,
        key: &MessageKey,
        state: ModerationState,
    ) -> Result<(), SocialError> {
        let message = self
            .messages
            .get_mut(key)
            .ok_or_else(|| SocialError::MessageNotFound(key.clone()))?;
        if message.moderation != ModerationState::Pending {
            return Err(SocialError::MessageNotPending(key.clone()));
        }
        message.moderation = state;
        self.pending.retain(|pending_key| pending_key != key);
        Ok(())
    }

    #[must_use]
    pub fn search(&self, filter: &MessageFilter) -> Vec<&SocialMessage> {
        let mut messages: Vec<_> = self
            .message_order
            .iter()
            .filter_map(|key| self.messages.get(key))
            .filter(|message| filter.matches(message))
            .collect();
        messages.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.key.cmp(&right.key))
        });
        if filter.order == SearchOrder::NewestFirst {
            messages.reverse();
        }
        if let Some(limit) = filter.limit {
            messages.truncate(limit);
        }
        messages
    }

    #[must_use]
    pub fn provider_state(&self, provider_id: &ProviderId) -> Option<&ProviderState> {
        self.providers.get(provider_id)
    }

    #[must_use]
    pub fn rate_records(&self) -> &[ProviderRateRecord] {
        &self.rate_records
    }

    #[must_use]
    pub fn backoff_records(&self) -> &[ProviderBackoffRecord] {
        &self.backoff_records
    }

    /// Polls an adapter and records provider state before ingesting its messages.
    ///
    /// # Errors
    ///
    /// Returns an authorization, rate limit, backoff, or transport provider error.
    pub fn poll_provider<P: SocialProvider>(
        &mut self,
        provider: &mut P,
        now: Timestamp,
    ) -> Result<PollReport, ProviderError> {
        let provider_id = provider.provider_id();
        let authorization = provider.authorization_state();
        let capabilities = provider.capabilities();
        let state = self
            .providers
            .entry(provider_id.clone())
            .or_insert_with(|| ProviderState {
                capabilities: capabilities.clone(),
                authorization: authorization.clone(),
                cursor: None,
                latest_rate: None,
                latest_backoff: None,
                last_error: None,
            });
        state.capabilities = capabilities;
        state.authorization = authorization.clone();

        if !authorization.permits_fetch() {
            let error = ProviderError::Authorization(authorization);
            state.last_error = Some(error.to_string());
            return Err(error);
        }

        let cursor = state.cursor.clone();
        let batch = match provider.fetch(cursor.as_deref(), now) {
            Ok(batch) => batch,
            Err(error) => {
                self.record_provider_error(&provider_id, &error);
                return Err(error);
            }
        };

        let ProviderBatch {
            mut messages,
            next_cursor,
            rate,
        } = batch;
        for message in &mut messages {
            message.provider_id = provider_id.clone();
        }
        if let Some(rate) = rate {
            self.record_rate(provider_id.clone(), rate);
        }
        let outcomes = self.ingest_batch(messages, now);
        if let Some(state) = self.providers.get_mut(&provider_id) {
            state.cursor.clone_from(&next_cursor);
            state.last_error = None;
        }

        Ok(PollReport {
            provider_id,
            outcomes,
            next_cursor,
        })
    }

    fn record_provider_error(&mut self, provider_id: &ProviderId, error: &ProviderError) {
        if let Some(state) = self.providers.get_mut(provider_id) {
            state.last_error = Some(error.to_string());
            if let ProviderError::Authorization(authorization) = error {
                state.authorization = authorization.clone();
            }
        }
        match error {
            ProviderError::RateLimited(rate) => {
                self.record_rate(provider_id.clone(), rate.clone());
            }
            ProviderError::Backoff(backoff) => {
                if let Some(state) = self.providers.get_mut(provider_id) {
                    state.latest_backoff = Some(backoff.clone());
                }
                self.backoff_records.push(ProviderBackoffRecord {
                    provider_id: provider_id.clone(),
                    backoff: backoff.clone(),
                });
            }
            ProviderError::Authorization(_) | ProviderError::Transport(_) => {}
        }
    }

    fn record_rate(&mut self, provider_id: ProviderId, rate: RateLimitRecord) {
        if let Some(state) = self.providers.get_mut(&provider_id) {
            state.latest_rate = Some(rate.clone());
        }
        self.rate_records
            .push(ProviderRateRecord { provider_id, rate });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeProvider {
        id: ProviderId,
        authorization: AuthorizationState,
        batches: VecDeque<Result<ProviderBatch, ProviderError>>,
        cursors: Vec<Option<String>>,
    }

    impl FakeProvider {
        fn new(batches: Vec<Result<ProviderBatch, ProviderError>>) -> Self {
            Self {
                id: ProviderId::from("fake"),
                authorization: AuthorizationState::Authorized,
                batches: batches.into(),
                cursors: Vec::new(),
            }
        }
    }

    impl SocialProvider for FakeProvider {
        fn provider_id(&self) -> ProviderId {
            self.id.clone()
        }

        fn capabilities(&self) -> BTreeSet<ProviderCapability> {
            [
                ProviderCapability::LiveMessages,
                ProviderCapability::RateLimitStatus,
            ]
            .into_iter()
            .collect()
        }

        fn authorization_state(&self) -> AuthorizationState {
            self.authorization.clone()
        }

        fn fetch(
            &mut self,
            cursor: Option<&str>,
            _now: Timestamp,
        ) -> Result<ProviderBatch, ProviderError> {
            self.cursors.push(cursor.map(str::to_owned));
            self.batches.pop_front().unwrap_or_else(|| {
                Ok(ProviderBatch {
                    next_cursor: cursor.map(str::to_owned),
                    ..ProviderBatch::default()
                })
            })
        }
    }

    fn incoming(id: &str, author: &str, body: &str, created_at: u64) -> IncomingMessage {
        IncomingMessage {
            provider_id: ProviderId::from("fake"),
            account_id: AccountId::from("channel"),
            message_id: MessageId::from(id),
            author_id: AuthorId::from(author),
            author_name: format!("Author {author}"),
            author_handle: Some(format!("@{author}")),
            body: body.to_owned(),
            created_at: Timestamp(created_at),
            expires_at: None,
        }
    }

    fn aggregator(capacity: usize) -> SocialAggregator {
        SocialAggregator::new(capacity, ContentPolicy::default())
    }

    #[test]
    fn fake_provider_orders_messages_and_suppresses_exact_duplicates() {
        let duplicate = incoming("one", "a", "first", 10);
        let batch = ProviderBatch {
            messages: vec![
                incoming("two", "b", "second", 20),
                duplicate.clone(),
                duplicate,
            ],
            next_cursor: Some("cursor-1".to_owned()),
            rate: None,
        };
        let mut provider = FakeProvider::new(vec![Ok(batch)]);
        let mut social = aggregator(10);

        let report = social
            .poll_provider(&mut provider, Timestamp(30))
            .expect("fake provider should poll");

        assert!(matches!(report.outcomes[0], Ok(IngestOutcome::Accepted(_))));
        assert!(matches!(
            report.outcomes[1],
            Ok(IngestOutcome::Duplicate(_))
        ));
        assert!(matches!(report.outcomes[2], Ok(IngestOutcome::Accepted(_))));
        let ids: Vec<_> = social
            .pending_messages()
            .iter()
            .map(|message| message.key.message_id.as_str())
            .collect();
        assert_eq!(ids, ["one", "two"]);
        assert_eq!(provider.cursors, [None]);
        assert_eq!(
            social
                .provider_state(&ProviderId::from("fake"))
                .unwrap()
                .cursor,
            Some("cursor-1".to_owned())
        );
    }

    #[test]
    fn moderation_transitions_release_bounded_queue_space() {
        let mut social = aggregator(1);
        let first = incoming("one", "a", "first", 1);
        let first_key = first.key();
        social.ingest(first, Timestamp(2)).unwrap();
        assert_eq!(
            social.ingest(incoming("two", "b", "second", 2), Timestamp(3)),
            Err(SocialError::QueueFull { capacity: 1 })
        );

        social.approve(&first_key).unwrap();
        let second = incoming("two", "b", "second", 2);
        let second_key = second.key();
        social.ingest(second, Timestamp(3)).unwrap();
        social.reject(&second_key, "off topic").unwrap();

        assert_eq!(
            social.message(&first_key).unwrap().moderation,
            ModerationState::Approved
        );
        assert_eq!(
            social.message(&second_key).unwrap().moderation,
            ModerationState::Rejected {
                reason: "off topic".to_owned()
            }
        );
        assert_eq!(social.pending_len(), 0);
    }

    #[test]
    fn block_applies_to_future_messages_from_author_on_same_account() {
        let mut social = aggregator(2);
        let first = incoming("one", "blocked-user", "first", 1);
        let first_key = first.key();
        social.ingest(first, Timestamp(1)).unwrap();
        social.block(&first_key).unwrap();

        let later = incoming("two", "blocked-user", "later", 2);
        let later_key = later.key();
        social.ingest(later, Timestamp(2)).unwrap();

        assert_eq!(
            social.message(&first_key).unwrap().moderation,
            ModerationState::Blocked
        );
        assert_eq!(
            social.message(&later_key).unwrap().moderation,
            ModerationState::Blocked
        );
        assert_eq!(social.pending_len(), 0);
    }

    #[test]
    fn profanity_and_literal_redaction_are_applied_before_moderation() {
        let policy = ContentPolicy {
            profanity: ProfanityPolicy {
                terms: ["heck".to_owned()].into_iter().collect(),
                action: ProfanityAction::Redact {
                    replacement: "[beep]".to_owned(),
                },
            },
            redactions: vec![RedactionRule::new("secret@example.test", "[email]")],
        };
        let mut social = SocialAggregator::new(2, policy);
        let message = incoming("one", "a", "HECK secret@example.test", 1);
        let key = message.key();
        social.ingest(message, Timestamp(1)).unwrap();

        let stored = social.message(&key).unwrap();
        assert_eq!(stored.body, "[beep] [email]");
        assert_eq!(
            stored.flags,
            [
                ModerationFlag::Profanity("heck".to_owned()),
                ModerationFlag::Redacted("secret@example.test".to_owned())
            ]
        );
    }

    #[test]
    fn expiry_removes_due_messages_from_moderation_queue() {
        let mut social = aggregator(2);
        let mut message = incoming("one", "a", "temporary", 1);
        message.expires_at = Some(Timestamp(10));
        let key = message.key();
        social.ingest(message, Timestamp(2)).unwrap();

        assert_eq!(social.expire_due(Timestamp(9)), 0);
        assert_eq!(social.expire_due(Timestamp(10)), 1);
        assert_eq!(
            social.message(&key).unwrap().moderation,
            ModerationState::Expired
        );
        assert!(social.pending_messages().is_empty());
    }

    #[test]
    fn search_and_title_mapping_use_moderated_fields() {
        let mut social = aggregator(2);
        let message = incoming("one", "alice", "Launch update", 7);
        let key = message.key();
        social.ingest(message, Timestamp(8)).unwrap();
        social.approve(&key).unwrap();

        let filter = MessageFilter {
            query: Some("launch".to_owned()),
            statuses: [ModerationStatus::Approved].into_iter().collect(),
            ..MessageFilter::default()
        };
        let results = social.search(&filter);
        assert_eq!(results.len(), 1);

        let mapping = TitleFieldMapping {
            bindings: vec![
                TitleFieldBinding {
                    title_field: "headline".to_owned(),
                    source: TitleSourceField::Body,
                },
                TitleFieldBinding {
                    title_field: "name".to_owned(),
                    source: TitleSourceField::AuthorName,
                },
                TitleFieldBinding {
                    title_field: "source_id".to_owned(),
                    source: TitleSourceField::MessageId,
                },
            ],
        };
        let fields = mapping.map(results[0]);
        assert_eq!(fields.get("headline").unwrap(), "Launch update");
        assert_eq!(fields.get("name").unwrap(), "Author alice");
        assert_eq!(fields.get("source_id").unwrap(), "one");
    }

    #[test]
    fn authorization_and_rate_failures_are_recorded() {
        let rate = RateLimitRecord {
            limit: Some(100),
            remaining: Some(0),
            reset_at: Some(Timestamp(50)),
            observed_at: Timestamp(10),
        };
        let mut provider = FakeProvider::new(vec![Err(ProviderError::RateLimited(rate.clone()))]);
        provider.authorization = AuthorizationState::Required;
        let mut social = aggregator(2);

        assert_eq!(
            social.poll_provider(&mut provider, Timestamp(10)),
            Err(ProviderError::Authorization(AuthorizationState::Required))
        );
        assert!(provider.cursors.is_empty());

        provider.authorization = AuthorizationState::Authorized;
        assert_eq!(
            social.poll_provider(&mut provider, Timestamp(10)),
            Err(ProviderError::RateLimited(rate.clone()))
        );
        assert_eq!(social.rate_records()[0].rate, rate);
        assert_eq!(
            social
                .provider_state(&ProviderId::from("fake"))
                .unwrap()
                .latest_rate,
            Some(rate)
        );
    }

    #[test]
    fn backoff_failures_are_recorded() {
        let backoff = BackoffRecord {
            attempt: 3,
            retry_at: Timestamp(80),
            reason: "temporary failure".to_owned(),
            observed_at: Timestamp(20),
        };
        let mut provider = FakeProvider::new(vec![Err(ProviderError::Backoff(backoff.clone()))]);
        let mut social = aggregator(2);

        assert_eq!(
            social.poll_provider(&mut provider, Timestamp(20)),
            Err(ProviderError::Backoff(backoff.clone()))
        );
        assert_eq!(social.backoff_records()[0].backoff, backoff);
    }
}
