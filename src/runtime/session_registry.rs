//! In-memory cache of each member's resumable backend session id, kept in sync
//! with the `agent_sessions` table so the runtime can resume without a DB hit.
//!
//! Next to each session id it keeps a watermark: the newest chat message id
//! the native session has already seen or produced. A prompt sent to the
//! session only needs the chat after that point (see `context_recap`).
//! Sessions of an earlier backend are parked here when a member switches CLI,
//! so switching back resumes them and recaps only what they missed.

use std::collections::HashMap;

use crate::domain::event::AgentSessionId;
use crate::domain::team::{BackendKind, MemberId, TeamMember};
use crate::store::sqlite::{SqliteStore, StoredConversationSession};

#[derive(Clone, Debug, Eq, PartialEq)]
struct Entry {
    session: AgentSessionId,
    /// `None` only for data saved before watermarks existed, until
    /// [`SessionRegistry::resolve_unknown_seen`] fills it in.
    seen: Option<i64>,
}

#[derive(Clone, Debug, Default)]
pub struct SessionRegistry {
    sessions: HashMap<MemberId, Entry>,
    parked: HashMap<(MemberId, BackendKind), Entry>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load any persisted sessions for the given members.
    pub fn from_store(store: &SqliteStore, members: &[TeamMember]) -> Self {
        let stored = store.stored_sessions().unwrap_or_default();
        Self::from_stored_for(&stored, members)
    }

    /// Like [`Self::from_stored`], for a roster that may have changed backend
    /// since the rows were saved. Native session ids are backend-scoped: a
    /// session saved for another backend than the member's current one is
    /// parked instead of resumed, and a parked one for the current backend is
    /// brought back.
    pub fn from_stored_for(stored: &[StoredConversationSession], members: &[TeamMember]) -> Self {
        let mut registry = Self::new();
        for saved in stored {
            let backend_differs = members
                .iter()
                .any(|member| member.id == saved.member && member.backend != saved.backend);
            registry.insert_stored(saved, saved.parked || backend_differs);
        }
        for member in members {
            if !registry.sessions.contains_key(&member.id)
                && let Some(entry) = registry.parked.remove(&(member.id.clone(), member.backend))
            {
                registry.sessions.insert(member.id.clone(), entry);
            }
        }
        registry
    }

    /// Rebuild the registry from stored rows (a snapshot or the tables).
    pub fn from_stored(stored: &[StoredConversationSession]) -> Self {
        let mut registry = Self::new();
        for saved in stored {
            registry.insert_stored(saved, saved.parked);
        }
        registry
    }

    fn insert_stored(&mut self, saved: &StoredConversationSession, parked: bool) {
        let entry = Entry {
            session: AgentSessionId(saved.session_id.clone()),
            seen: saved.seen_through,
        };
        if parked {
            self.parked
                .insert((saved.member.clone(), saved.backend), entry);
        } else {
            self.sessions.insert(saved.member.clone(), entry);
        }
    }

    /// The rows to store for `members`: each member's active session on its
    /// current backend, plus its parked sessions of other backends.
    pub fn to_stored(&self, members: &[TeamMember]) -> Vec<StoredConversationSession> {
        let mut stored = Vec::new();
        for member in members {
            if let Some(entry) = self.sessions.get(&member.id) {
                stored.push(StoredConversationSession {
                    member: member.id.clone(),
                    backend: member.backend,
                    session_id: entry.session.0.clone(),
                    seen_through: entry.seen,
                    parked: false,
                });
            }
        }
        let mut parked: Vec<_> = self
            .parked
            .iter()
            .filter(|((id, backend), _)| {
                members
                    .iter()
                    .any(|member| &member.id == id && member.backend != *backend)
            })
            .collect();
        parked.sort_by_key(|((id, backend), _)| (id.as_str().to_string(), backend.as_str()));
        for ((id, backend), entry) in parked {
            stored.push(StoredConversationSession {
                member: id.clone(),
                backend: *backend,
                session_id: entry.session.0.clone(),
                seen_through: entry.seen,
                parked: true,
            });
        }
        stored
    }

    pub fn get(&self, member: &MemberId) -> Option<AgentSessionId> {
        self.sessions.get(member).map(|entry| entry.session.clone())
    }

    /// Use `session` for the member. The same id keeps its watermark; a new id
    /// is a new native conversation that has seen nothing yet.
    pub fn set(&mut self, member: MemberId, session: AgentSessionId) {
        match self.sessions.get_mut(&member) {
            Some(entry) if entry.session == session => {}
            _ => {
                self.sessions.insert(
                    member,
                    Entry {
                        session,
                        seen: Some(0),
                    },
                );
            }
        }
    }

    pub fn clear(&mut self, member: &MemberId) {
        self.sessions.remove(member);
    }

    /// How far into the chat the member's current session has seen. 0 when it
    /// has no session: a CLI that starts fresh has seen nothing.
    pub fn seen(&self, member: &MemberId) -> i64 {
        self.sessions
            .get(member)
            .and_then(|entry| entry.seen)
            .unwrap_or(0)
    }

    /// Record that the member's current session has seen the chat up to
    /// `message_id`. The watermark only moves forward.
    pub fn mark_seen(&mut self, member: &MemberId, message_id: i64) {
        if let Some(entry) = self.sessions.get_mut(member) {
            entry.seen = Some(entry.seen.unwrap_or(0).max(message_id));
        }
    }

    /// A session handed in from outside (an attached or configured native
    /// session) carries its own history: treat the chat so far as seen.
    pub fn set_external(&mut self, member: MemberId, session: AgentSessionId, message_id: i64) {
        self.sessions.insert(
            member,
            Entry {
                session,
                seen: Some(message_id),
            },
        );
    }

    /// Give sessions saved before watermarks existed one: they are assumed to
    /// have seen the chat up to `message_id`. Returns whether any changed.
    pub fn resolve_unknown_seen(&mut self, message_id: i64) -> bool {
        let mut changed = false;
        for entry in self
            .sessions
            .values_mut()
            .chain(self.parked.values_mut())
            .filter(|entry| entry.seen.is_none())
        {
            entry.seen = Some(message_id);
            changed = true;
        }
        changed
    }

    /// The member switched from `from` to `to`: put the old session aside and
    /// bring back a parked session of `to`, if it has one.
    pub fn switch_backend(&mut self, member: &MemberId, from: BackendKind, to: BackendKind) {
        if let Some(entry) = self.sessions.remove(member) {
            self.parked.insert((member.clone(), from), entry);
        }
        if let Some(entry) = self.parked.remove(&(member.clone(), to)) {
            self.sessions.insert(member.clone(), entry);
        }
    }

    /// Forget a member's parked sessions (it was removed from the team).
    pub fn drop_member(&mut self, member: &MemberId) {
        self.sessions.remove(member);
        self.parked.retain(|(id, _), _| id != member);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::team::BackendKind;

    #[test]
    fn loads_and_caches_sessions() {
        let store = SqliteStore::in_memory().unwrap();
        let builder = MemberId::new("builder");
        store
            .upsert_session(
                &builder,
                BackendKind::Codex,
                &AgentSessionId("t-1".to_string()),
            )
            .unwrap();

        let member = TeamMember::new(
            builder.clone(),
            "Builder",
            BackendKind::Codex,
            "implementation",
        );
        let mut registry = SessionRegistry::from_store(&store, &[member]);
        assert_eq!(
            registry.get(&builder),
            Some(AgentSessionId("t-1".to_string()))
        );

        registry.set(builder.clone(), AgentSessionId("t-2".to_string()));
        assert_eq!(
            registry.get(&builder),
            Some(AgentSessionId("t-2".to_string()))
        );

        registry.clear(&builder);
        assert_eq!(registry.get(&builder), None);
    }

    #[test]
    fn does_not_reuse_a_session_from_another_backend() {
        let store = SqliteStore::in_memory().unwrap();
        let builder = MemberId::new("builder");
        store
            .upsert_session(
                &builder,
                BackendKind::Codex,
                &AgentSessionId("codex-thread".to_string()),
            )
            .unwrap();
        let member = TeamMember::new(
            builder.clone(),
            "Builder",
            BackendKind::Claude,
            "implementation",
        );

        let registry = SessionRegistry::from_store(&store, &[member]);

        assert_eq!(registry.get(&builder), None);
    }
}
