//! Ephemeral presence for a hosted room: who is here, and what they publish.
//!
//! Extracted unchanged from `Room` so a Y.Doc room and a ledger room share one implementation.
//! Awareness is connection state, never durable state: every live entry has an owning socket, JSON
//! `null` becomes an invisible clock tombstone that late joiners never replay (retaining the clock
//! is what stops a delayed pre-close frame from resurrecting a removed collaborator), and
//! Shrubbery's stable `(human, device, tab)` key with a monotonic connection epoch lets a newer
//! incarnation of one tab supersede the old one while two tabs of the same human stay independent.
//!
//! `yrs::sync::Awareness` is `!Send`, so the table tracks merged `AwarenessUpdateEntry` values (plain
//! `Send` data): the server is a relay and a late-joiner answerer, not an awareness participant.
//!
//! Every method is synchronous: the two mutexes are held only for the in-memory merge and never
//! across an await point.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use serde::Deserialize;
use yrs::block::ClientID;
use yrs::sync::awareness::AwarenessUpdateEntry;
use yrs::sync::{AwarenessUpdate, Message};
use yrs::updates::encoder::Encode;

use super::rooms::ConnectionId;

/// One client id's latest awareness state, with the socket responsible for it.
#[derive(Clone)]
pub(crate) struct RoomAwarenessEntry {
    /// The protocol entry (clock and JSON state).
    pub(crate) entry: AwarenessUpdateEntry,
    /// The websocket currently responsible for a live state. Tombstones keep only the protocol
    /// clock, so they are never replayed as a collaborator.
    pub(crate) owner: Option<ConnectionId>,
    /// The logical browser tab this state belongs to, when the client declared one.
    pub(crate) session: Option<PresenceSession>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PresenceSession {
    key: PresenceSessionKey,
    connection_epoch: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct PresenceSessionKey {
    human_id: String,
    device_id: String,
    client_id: String,
}

/// High-water state for one logical browser tab while any socket that has participated in that
/// tab's lifecycle is still connected. Keeping each owner's own epoch prevents an older socket from
/// claiming a newer epoch during the small gap between replacement removal and transport teardown.
pub(crate) struct PresenceSessionLease {
    high_water_epoch: u64,
    owner_epochs: HashMap<ConnectionId, u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PresenceEnvelope {
    presence: Option<PresenceWireIdentity>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PresenceWireIdentity {
    human_id: String,
    device_id: String,
    client_id: String,
    connection_epoch: u64,
}

fn presence_session(entry: &AwarenessUpdateEntry) -> Option<PresenceSession> {
    let envelope: PresenceEnvelope = serde_json::from_str(entry.json.as_ref()).ok()?;
    let identity = envelope.presence?;
    if identity.human_id.trim().is_empty()
        || identity.device_id.trim().is_empty()
        || identity.client_id.trim().is_empty()
    {
        return None;
    }
    Some(PresenceSession {
        key: PresenceSessionKey {
            human_id: identity.human_id,
            device_id: identity.device_id,
            client_id: identity.client_id,
        },
        connection_epoch: identity.connection_epoch,
    })
}

fn is_awareness_removal(entry: &AwarenessUpdateEntry) -> bool {
    entry.json.trim() == "null"
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// The awareness state of one room: the merged entries and the presence-session leases.
#[derive(Default)]
pub(crate) struct AwarenessTable {
    state: Mutex<HashMap<ClientID, RoomAwarenessEntry>>,
    sessions: Mutex<HashMap<PresenceSessionKey, PresenceSessionLease>>,
}

impl AwarenessTable {
    /// An empty table.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The entries of `update` whose client id no other live socket holds (a visitor's awareness;
    /// `None` when nothing is left).
    pub(crate) fn held_by_this_socket(
        &self,
        mut update: AwarenessUpdate,
        owner: ConnectionId,
    ) -> Option<AwarenessUpdate> {
        let state = locked(&self.state);
        update.clients.retain(|client, _| {
            !state
                .get(client)
                .is_some_and(|existing| existing.owner.is_some_and(|holder| holder != owner))
        });
        (!update.clients.is_empty()).then_some(update)
    }

    /// Merge one websocket's awareness update into the live state and return the encoded awareness
    /// message that is safe to broadcast (`Message::Awareness`, y-sync tag `1`), or an empty vector
    /// when nothing was accepted. See [`Self::merge_detailed`] for the rules and for whether the
    /// sender may skip its echo.
    pub(crate) fn merge_tagged(&self, update: AwarenessUpdate, from: ConnectionId) -> Vec<u8> {
        match self.merge_detailed(&update, from) {
            Some((accepted, _own_only)) => Message::Awareness(accepted).encode_v1(),
            None => Vec::new(),
        }
    }

    /// Merge one websocket's awareness update into the room's live state.
    ///
    /// Awareness is ephemeral connection state, not durable document state: every live entry has an
    /// owning socket. JSON `null` becomes an invisible clock tombstone, never something late joiners
    /// replay; retaining that clock is what prevents delayed pre-close frames from resurrecting a
    /// removed collaborator. Shrubbery additionally supplies a stable human/device/tab key and a
    /// monotonically increasing connection epoch. A newer incarnation supersedes that exact tab
    /// only; two tabs belonging to the same human remain independently present.
    ///
    /// The returned update is the canonical subset safe to broadcast. It may include synthetic
    /// clock+1 removals for client ids superseded by a newer incarnation. Stale or non-owning
    /// frames are omitted. The flag says whether every accepted entry belongs to `owner`'s own live
    /// client ids; only then may the sender skip its echo, because synthetic removals of superseded
    /// ids are news to the sender too.
    pub(crate) fn merge_detailed(
        &self,
        update: &AwarenessUpdate,
        owner: ConnectionId,
    ) -> Option<(AwarenessUpdate, bool)> {
        let mut state = locked(&self.state);
        let mut sessions = locked(&self.sessions);
        let mut accepted = HashMap::new();

        for (client, entry) in &update.clients {
            if is_awareness_removal(entry) {
                let existing = state.get(client).cloned();
                if let Some(current) = existing.as_ref() {
                    if current.entry.clock > entry.clock {
                        continue;
                    }
                    if current
                        .owner
                        .is_some_and(|current_owner| current_owner != owner)
                    {
                        // A delayed close from an old socket must never delete
                        // live state already transferred to its replacement.
                        continue;
                    }
                    if current.owner.is_none() && current.entry.clock >= entry.clock {
                        continue;
                    }
                }

                if let Some(session) = existing.and_then(|current| current.session) {
                    let owner_still_live = state.iter().any(|(other_client, live)| {
                        other_client != client
                            && live.owner == Some(owner)
                            && live
                                .session
                                .as_ref()
                                .is_some_and(|candidate| candidate.key == session.key)
                    });
                    if !owner_still_live {
                        let drop_lease = if let Some(lease) = sessions.get_mut(&session.key) {
                            lease.owner_epochs.remove(&owner);
                            lease.owner_epochs.is_empty()
                        } else {
                            false
                        };
                        if drop_lease {
                            sessions.remove(&session.key);
                        }
                    }
                }
                state.insert(
                    *client,
                    RoomAwarenessEntry {
                        entry: entry.clone(),
                        owner: None,
                        session: None,
                    },
                );
                accepted.insert(*client, entry.clone());
                continue;
            }

            // Awareness clocks are the protocol-level ordering for one Yjs
            // client ID. Validate this before mutating logical session state.
            if state
                .get(client)
                .is_some_and(|existing| existing.entry.clock >= entry.clock)
            {
                continue;
            }

            let incoming_session = presence_session(entry);
            if let Some(incoming) = incoming_session.as_ref() {
                if let Some(lease) = sessions.get(&incoming.key) {
                    if incoming.connection_epoch < lease.high_water_epoch {
                        continue;
                    }
                    if incoming.connection_epoch == lease.high_water_epoch {
                        let owner_is_current = lease
                            .owner_epochs
                            .get(&owner)
                            .is_some_and(|epoch| *epoch == incoming.connection_epoch);
                        let live_for_key = state.iter().find_map(|(other_client, existing)| {
                            existing.session.as_ref().and_then(|session| {
                                (session.key == incoming.key)
                                    .then_some((*other_client, existing.owner))
                            })
                        });
                        let live_is_compatible = match live_for_key {
                            Some((live_client, live_owner)) => {
                                live_client == *client && live_owner == Some(owner)
                            }
                            None => owner_is_current,
                        };
                        if !owner_is_current || !live_is_compatible {
                            // Equal epochs are heartbeats/republication by the
                            // already-known socket, never ownership transfer.
                            continue;
                        }
                    }
                }

                let superseded: Vec<_> = state
                    .iter()
                    .filter_map(|(other_client, existing)| {
                        existing.session.as_ref().and_then(|session| {
                            (session.key == incoming.key
                                && session.connection_epoch < incoming.connection_epoch)
                                .then_some((*other_client, existing.entry.clock))
                        })
                    })
                    .collect();
                for (other_client, clock) in superseded {
                    state.insert(
                        other_client,
                        RoomAwarenessEntry {
                            entry: AwarenessUpdateEntry {
                                clock: clock.saturating_add(1),
                                json: "null".into(),
                            },
                            owner: None,
                            session: None,
                        },
                    );
                    if other_client != *client {
                        accepted.insert(
                            other_client,
                            AwarenessUpdateEntry {
                                clock: clock.saturating_add(1),
                                json: "null".into(),
                            },
                        );
                    }
                }

                let lease =
                    sessions
                        .entry(incoming.key.clone())
                        .or_insert_with(|| PresenceSessionLease {
                            high_water_epoch: incoming.connection_epoch,
                            owner_epochs: HashMap::new(),
                        });
                lease.high_water_epoch = lease.high_water_epoch.max(incoming.connection_epoch);
                lease.owner_epochs.insert(owner, incoming.connection_epoch);
            }

            state.insert(
                *client,
                RoomAwarenessEntry {
                    entry: entry.clone(),
                    owner: Some(owner),
                    session: incoming_session,
                },
            );
            accepted.insert(*client, entry.clone());
        }

        if accepted.is_empty() {
            None
        } else {
            let own_only = accepted.keys().all(|client| {
                state
                    .get(client)
                    .is_some_and(|live| live.owner == Some(owner))
            });
            Some((AwarenessUpdate { clients: accepted }, own_only))
        }
    }

    /// Every live (non-tombstone) entry, for a late joiner; `None` when nobody is present.
    pub(crate) fn full(&self) -> Option<AwarenessUpdate> {
        let state = locked(&self.state);
        let clients = state
            .iter()
            .filter_map(|(client, live)| {
                (!is_awareness_removal(&live.entry)).then_some((*client, live.entry.clone()))
            })
            .collect::<HashMap<_, _>>();
        (!clients.is_empty()).then_some(AwarenessUpdate { clients })
    }

    /// [`Self::full`] as the encoded awareness message (`Message::Awareness`), ready to send.
    pub(crate) fn encode_all(&self) -> Option<Vec<u8>> {
        self.full()
            .map(|update| Message::Awareness(update).encode_v1())
    }

    /// Remove every awareness entry still owned by one websocket. Ownership may already have moved
    /// to a newer incarnation; those entries survive. Returns the synthetic removals to broadcast,
    /// or `None` when the socket owned nothing.
    pub(crate) fn remove_connection(&self, from: ConnectionId) -> Option<AwarenessUpdate> {
        let mut state = locked(&self.state);
        let mut sessions = locked(&self.sessions);
        let owned: Vec<_> = state
            .iter()
            .filter_map(|(client, live)| {
                (live.owner == Some(from)).then_some((*client, live.entry.clock))
            })
            .collect();
        let mut clients = HashMap::with_capacity(owned.len());
        for (client, clock) in owned {
            let tombstone = AwarenessUpdateEntry {
                clock: clock.saturating_add(1),
                json: "null".into(),
            };
            state.insert(
                client,
                RoomAwarenessEntry {
                    entry: tombstone.clone(),
                    owner: None,
                    session: None,
                },
            );
            clients.insert(client, tombstone);
        }
        sessions.retain(|_, lease| {
            lease.owner_epochs.remove(&from);
            !lease.owner_epochs.is_empty()
        });
        if clients.is_empty() {
            None
        } else {
            Some(AwarenessUpdate { clients })
        }
    }

    /// The raw entry map, for tests that assert on ownership and clocks.
    #[cfg(test)]
    pub(crate) fn entries_for_test(&self) -> MutexGuard<'_, HashMap<ClientID, RoomAwarenessEntry>> {
        locked(&self.state)
    }
}
