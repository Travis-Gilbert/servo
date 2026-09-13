/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The Web Locks registry: one lock request queue and one held lock set per
//! origin, owned by the constellation so that every agent of an origin
//! (documents in any pipeline, dedicated and shared workers) sees the same
//! state.
//!
//! <https://w3c.github.io/web-locks/#lock-managers>

use std::collections::HashMap;

use log::warn;
use servo_base::generic_channel::GenericCallback;
use servo_base::id::PipelineId;
use servo_constellation_traits::{WebLockInfo, WebLockMessage, WebLockMode, WebLockResponse};
use servo_url::ImmutableOrigin;

/// The identity of a lock request within its origin: the requesting client
/// plus the id that client allocated for the request.
#[derive(Clone, Debug, Eq, PartialEq)]
struct RequestKey {
    client_id: String,
    request_id: u64,
}

/// A lock request that has not been granted yet, or a held lock.
/// <https://w3c.github.io/web-locks/#lock-request> and
/// <https://w3c.github.io/web-locks/#lock>
#[derive(Debug)]
struct LockEntry {
    key: RequestKey,
    /// The pipeline the request arrived from, when the requesting agent dies
    /// with it. Used as a backstop to release locks when a pipeline exits
    /// without sending `ClientGone`. None for a shared worker, which outlives
    /// the document that created it.
    pipeline_id: Option<PipelineId>,
    name: String,
    mode: WebLockMode,
    result_handler: GenericCallback<WebLockResponse>,
}

impl LockEntry {
    fn info(&self) -> WebLockInfo {
        WebLockInfo {
            name: self.name.clone(),
            mode: self.mode,
            client_id: self.key.client_id.clone(),
        }
    }

    fn reply(&self, response: WebLockResponse) {
        if let Err(error) = self.result_handler.send(response) {
            warn!("Failed to deliver a Web Locks response: {error:?}");
        }
    }
}

/// <https://w3c.github.io/web-locks/#lock-manager>
#[derive(Debug, Default)]
struct OriginLocks {
    /// <https://w3c.github.io/web-locks/#lock-request-queue>
    queue: Vec<LockEntry>,
    /// <https://w3c.github.io/web-locks/#held-lock-set>
    held: Vec<LockEntry>,
}

impl OriginLocks {
    fn is_empty(&self) -> bool {
        self.queue.is_empty() && self.held.is_empty()
    }

    /// <https://w3c.github.io/web-locks/#lock-request-grantable>
    fn is_grantable(&self, index: usize) -> bool {
        let request = &self.queue[index];
        let earlier = &self.queue[..index];
        match request.mode {
            WebLockMode::Exclusive => {
                !self.held.iter().any(|lock| lock.name == request.name) &&
                    !earlier.iter().any(|other| other.name == request.name)
            },
            WebLockMode::Shared => {
                !self
                    .held
                    .iter()
                    .any(|lock| lock.mode == WebLockMode::Exclusive && lock.name == request.name) &&
                    !earlier.iter().any(|other| {
                        other.mode == WebLockMode::Exclusive && other.name == request.name
                    })
            },
        }
    }

    /// <https://w3c.github.io/web-locks/#process-the-lock-request-queue>
    fn process_queue(&mut self) {
        let mut index = 0;
        while index < self.queue.len() {
            if self.is_grantable(index) {
                let request = self.queue.remove(index);
                request.reply(WebLockResponse::Granted {
                    request_id: request.key.request_id,
                });
                self.held.push(request);
            } else {
                index += 1;
            }
        }
    }

    /// Remove every held lock matching `predicate`, leaving queued requests alone.
    fn remove_held_where(&mut self, predicate: impl Fn(&LockEntry) -> bool) -> Vec<LockEntry> {
        let mut removed = Vec::new();
        let mut index = 0;
        while index < self.held.len() {
            if predicate(&self.held[index]) {
                removed.push(self.held.remove(index));
            } else {
                index += 1;
            }
        }
        removed
    }

    /// Remove every held lock and queued request matching `predicate`.
    fn remove_where(&mut self, predicate: impl Fn(&LockEntry) -> bool) -> Vec<LockEntry> {
        let removed = self.remove_held_where(&predicate);
        self.queue.retain(|entry| !predicate(entry));
        removed
    }
}

/// Every origin's lock manager, keyed by origin.
#[derive(Debug, Default)]
pub(crate) struct WebLockRegistry {
    origins: HashMap<ImmutableOrigin, OriginLocks>,
}

impl WebLockRegistry {
    pub(crate) fn handle_message(&mut self, pipeline_id: PipelineId, message: WebLockMessage) {
        match message {
            WebLockMessage::Request {
                origin,
                client_id,
                request_id,
                name,
                mode,
                if_available,
                steal,
                pipeline_bound,
                result_handler,
            } => {
                let entry = LockEntry {
                    key: RequestKey {
                        client_id,
                        request_id,
                    },
                    pipeline_id: pipeline_bound.then_some(pipeline_id),
                    name,
                    mode,
                    result_handler,
                };
                self.request(origin, entry, if_available, steal);
            },
            WebLockMessage::Abort {
                origin,
                client_id,
                request_id,
            } => {
                let key = RequestKey {
                    client_id,
                    request_id,
                };
                self.with_origin(origin, |locks| {
                    // <https://w3c.github.io/web-locks/#abort-the-request>
                    // Only a request still in the queue can be aborted; a lock that
                    // was granted in the meantime is released by the client.
                    locks.queue.retain(|entry| entry.key != key);
                    locks.process_queue();
                });
            },
            WebLockMessage::Release {
                origin,
                client_id,
                request_id,
            } => {
                let key = RequestKey {
                    client_id,
                    request_id,
                };
                self.with_origin(origin, |locks| {
                    // <https://w3c.github.io/web-locks/#release-the-lock>
                    locks.held.retain(|entry| entry.key != key);
                    locks.process_queue();
                });
            },
            WebLockMessage::Query {
                origin,
                request_id,
                result_handler,
            } => {
                // <https://w3c.github.io/web-locks/#snapshot-the-lock-state>
                let (held, pending) = match self.origins.get(&origin) {
                    Some(locks) => (
                        locks.held.iter().map(LockEntry::info).collect(),
                        locks.queue.iter().map(LockEntry::info).collect(),
                    ),
                    None => (Vec::new(), Vec::new()),
                };
                if let Err(error) = result_handler.send(WebLockResponse::Snapshot {
                    request_id,
                    held,
                    pending,
                }) {
                    warn!("Failed to deliver a Web Locks snapshot: {error:?}");
                }
            },
            WebLockMessage::ClientGone { origin, client_id } => {
                self.with_origin(origin, |locks| {
                    locks.remove_where(|entry| entry.key.client_id == client_id);
                    locks.process_queue();
                });
            },
        }
    }

    /// Release every lock and drop every request that arrived from `pipeline_id`.
    /// This is the backstop for a pipeline that exits without its globals
    /// sending `ClientGone`, for example after a script thread panic.
    pub(crate) fn pipeline_exited(&mut self, pipeline_id: PipelineId) {
        for locks in self.origins.values_mut() {
            locks.remove_where(|entry| entry.pipeline_id == Some(pipeline_id));
            locks.process_queue();
        }
        self.origins.retain(|_, locks| !locks.is_empty());
    }

    /// <https://w3c.github.io/web-locks/#request-a-lock>
    fn request(&mut self, origin: ImmutableOrigin, entry: LockEntry, if_available: bool, steal: bool) {
        let locks = self.origins.entry(origin).or_default();
        if steal {
            // Step 5.1. For each lock of held with the same name: remove it and
            // reject its waiting promise with an "AbortError" DOMException.
            // Queued requests with that name stay queued.
            let name = entry.name.clone();
            for stolen in locks.remove_held_where(|held| held.name == name) {
                stolen.reply(WebLockResponse::Stolen {
                    request_id: stolen.key.request_id,
                });
            }
            // Step 5.2. Prepend request in queue.
            locks.queue.insert(0, entry);
        } else if if_available {
            // Step 6. If ifAvailable is true and request is not grantable, invoke
            // the callback with null. Grantability is tested as if request were
            // at the end of the queue.
            locks.queue.push(entry);
            let index = locks.queue.len() - 1;
            if !locks.is_grantable(index) {
                let request = locks.queue.remove(index);
                request.reply(WebLockResponse::Unavailable {
                    request_id: request.key.request_id,
                });
                return;
            }
        } else {
            // Step 7. Enqueue request in queue.
            locks.queue.push(entry);
        }
        // Step 8. Process the lock request queue for origin.
        locks.process_queue();
    }

    fn with_origin(&mut self, origin: ImmutableOrigin, operation: impl FnOnce(&mut OriginLocks)) {
        let Some(locks) = self.origins.get_mut(&origin) else {
            return;
        };
        operation(locks);
        if locks.is_empty() {
            self.origins.remove(&origin);
        }
    }
}
