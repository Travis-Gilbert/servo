/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

mod engines;

use std::borrow::ToOwned;
use std::collections::hash_map::Entry;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use log::warn;
use malloc_size_of::MallocSizeOf;
use malloc_size_of_derive::MallocSizeOf;
use net_traits::pub_domains::registered_domain_name;
use profile_traits::mem::{
    ProcessReports, ProfilerChan as MemProfilerChan, Report, ReportKind, perform_memory_report,
};
use profile_traits::path;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use servo_base::generic_channel::{self, GenericReceiver, GenericSender};
use servo_base::id::WebViewId;
use servo_base::threadpool::ThreadPool;
use servo_base::{read_json_from_file, write_json_to_file};
use servo_url::{ImmutableOrigin, ServoUrl};
use storage_traits::webstorage_thread::{
    OriginDescriptor, OriginEntry, WebStorageEngine, WebStorageEngineFactory, WebStorageThreadMsg,
    WebStorageType,
};
use uuid::Uuid;

use crate::webstorage::engines::sqlite::SqliteEngine;

const QUOTA_SIZE_LIMIT: usize = 5 * 1024 * 1024;

pub trait WebStorageThreadFactory {
    fn new(
        config_dir: Option<PathBuf>,
        mem_profiler_chan: MemProfilerChan,
        reporter_name: String,
        factory: Option<Arc<dyn WebStorageEngineFactory>>,
    ) -> Self;
}

impl WebStorageThreadFactory for GenericSender<WebStorageThreadMsg> {
    /// Create a storage thread
    fn new(
        config_dir: Option<PathBuf>,
        mem_profiler_chan: MemProfilerChan,
        reporter_name: String,
        factory: Option<Arc<dyn WebStorageEngineFactory>>,
    ) -> GenericSender<WebStorageThreadMsg> {
        let (chan, port) = generic_channel::channel().unwrap();
        let chan2 = chan.clone();
        thread::Builder::new()
            .name("WebStorageManager".to_owned())
            .spawn(move || {
                mem_profiler_chan.run_with_memory_reporting(
                    || WebStorageManager::new(port, config_dir, factory).start(),
                    reporter_name,
                    chan2,
                    WebStorageThreadMsg::CollectMemoryReport,
                );
            })
            .expect("Thread spawning failed");
        chan
    }
}

pub(crate) struct SqliteWebStorageEngineFactory;

impl WebStorageEngineFactory for SqliteWebStorageEngineFactory {
    fn open(
        &self,
        storage_type: WebStorageType,
        webview_id: Option<WebViewId>,
        _origin: &ImmutableOrigin,
        db_dir: Option<PathBuf>,
    ) -> Result<Box<dyn WebStorageEngine>, String> {
        debug_assert_eq!(storage_type, WebStorageType::Local);
        debug_assert!(webview_id.is_none());
        SqliteEngine::new(&db_dir, ThreadPool::global())
            .map(|engine| Box::new(engine) as Box<dyn WebStorageEngine>)
            .map_err(|error| error.to_string())
    }
}

#[derive(Deserialize, MallocSizeOf, Serialize)]
pub struct StorageOrigins {
    // TODO: Consider grouping by eTLD+1
    // TODO: Consider ImmutableOrigin instead of String for tracking origins
    origin_descriptors: FxHashMap<String, OriginDescriptor>,
}

impl StorageOrigins {
    fn new() -> Self {
        StorageOrigins {
            origin_descriptors: FxHashMap::default(),
        }
    }

    /// Ensures that an origin descriptor exists for the given origin.
    ///
    /// Returns `true` if a new origin descriptor was created, or `false` if
    /// one already existed.
    fn ensure_origin_descriptor(&mut self, origin: &ImmutableOrigin) -> bool {
        let origin = origin.ascii_serialization();
        match self.origin_descriptors.entry(origin.clone()) {
            Entry::Occupied(_) => false,
            Entry::Vacant(entry) => {
                entry.insert(OriginDescriptor::new(origin));
                true
            },
        }
    }

    fn origin_descriptors(&self) -> Vec<OriginDescriptor> {
        self.origin_descriptors.values().cloned().collect()
    }

    fn take_origins_for_sites(&mut self, sites: &[String]) -> Vec<ImmutableOrigin> {
        // TODO: This can use `extract_if` once MSVR is bumbed (>=1.88)

        let mut result = Vec::new();

        self.origin_descriptors.retain(|_, descriptor| {
            let url =
                ServoUrl::parse(&descriptor.name).expect("Should always be able to parse origins.");

            let Some(domain) = registered_domain_name(&url) else {
                warn!("Failed to get a registered domain name for: {url}");
                return true;
            };
            let domain = domain.to_string();

            if sites.contains(&domain) {
                result.push(url.origin());
                false
            } else {
                true
            }
        });

        result
    }
}

struct WebStorageEnvironment {
    engine: Box<dyn WebStorageEngine>,
}

impl MallocSizeOf for WebStorageEnvironment {
    fn size_of(&self, _ops: &mut malloc_size_of::MallocSizeOfOps) -> usize {
        0
    }
}

impl WebStorageEnvironment {
    fn new(engine: Box<dyn WebStorageEngine>) -> Self {
        WebStorageEnvironment { engine }
    }
}

struct WebStorageManager {
    port: GenericReceiver<WebStorageThreadMsg>,
    session_storage_origins: StorageOrigins,
    local_storage_origins: StorageOrigins,
    session_data: FxHashMap<WebViewId, FxHashMap<ImmutableOrigin, OriginEntry>>,
    session_environments: FxHashMap<WebViewId, FxHashMap<ImmutableOrigin, WebStorageEnvironment>>,
    config_dir: Option<PathBuf>,
    engine_factory: Arc<dyn WebStorageEngineFactory>,
    use_engine_for_session: bool,
    environments: FxHashMap<ImmutableOrigin, WebStorageEnvironment>,
}

impl WebStorageManager {
    fn new(
        port: GenericReceiver<WebStorageThreadMsg>,
        config_dir: Option<PathBuf>,
        engine_factory: Option<Arc<dyn WebStorageEngineFactory>>,
    ) -> WebStorageManager {
        let mut local_storage_origins = StorageOrigins::new();
        if let Some(ref config_dir) = config_dir {
            read_json_from_file(&mut local_storage_origins, config_dir, "localstorage.json");
        }
        let use_engine_for_session = engine_factory.is_some();
        WebStorageManager {
            port,
            session_storage_origins: StorageOrigins::new(),
            local_storage_origins,
            session_data: FxHashMap::default(),
            session_environments: FxHashMap::default(),
            config_dir,
            engine_factory: engine_factory
                .unwrap_or_else(|| Arc::new(SqliteWebStorageEngineFactory)),
            use_engine_for_session,
            environments: FxHashMap::default(),
        }
    }
}

impl WebStorageManager {
    fn start(&mut self) {
        loop {
            match self.port.recv().unwrap() {
                WebStorageThreadMsg::Length(sender, storage_type, webview_id, url) => {
                    self.length(sender, storage_type, webview_id, url)
                },
                WebStorageThreadMsg::Key(sender, storage_type, webview_id, url, index) => {
                    self.key(sender, storage_type, webview_id, url, index)
                },
                WebStorageThreadMsg::Keys(sender, storage_type, webview_id, url) => {
                    self.keys(sender, storage_type, webview_id, url)
                },
                WebStorageThreadMsg::SetItem(
                    sender,
                    storage_type,
                    webview_id,
                    url,
                    name,
                    value,
                ) => {
                    self.set_item(sender, storage_type, webview_id, url, name, value);
                },
                WebStorageThreadMsg::GetItem(sender, storage_type, webview_id, url, name) => {
                    self.request_item(sender, storage_type, webview_id, url, name)
                },
                WebStorageThreadMsg::RemoveItem(sender, storage_type, webview_id, url, name) => {
                    self.remove_item(sender, storage_type, webview_id, url, name);
                },
                WebStorageThreadMsg::Clear(sender, storage_type, webview_id, url) => {
                    self.clear(sender, storage_type, webview_id, url);
                },
                WebStorageThreadMsg::Clone {
                    sender,
                    src: src_webview_id,
                    dest: dest_webview_id,
                } => {
                    self.clone(src_webview_id, dest_webview_id);
                    let _ = sender.send(());
                },
                WebStorageThreadMsg::ListOrigins(sender, storage_type) => {
                    let _ = sender.send(self.origin_descriptors(storage_type));
                },
                WebStorageThreadMsg::ClearDataForSites(sender, storage_type, sites) => {
                    self.clear_data_for_sites(storage_type, &sites);
                    let _ = sender.send(());
                },
                WebStorageThreadMsg::CollectMemoryReport(sender) => {
                    let reports = self.collect_memory_reports();
                    sender.send(ProcessReports::new(reports));
                },
                WebStorageThreadMsg::Exit(sender) => {
                    // Nothing to do since we save localstorage set eagerly.
                    let _ = sender.send(());
                    break;
                },
            }
        }
    }

    fn collect_memory_reports(&self) -> Vec<Report> {
        let mut reports = vec![];
        perform_memory_report(|ops| {
            reports.push(Report {
                path: path!["storage", "local"],
                kind: ReportKind::ExplicitJemallocHeapSize,
                size: self.environments.size_of(ops) + self.local_storage_origins.size_of(ops),
            });

            reports.push(Report {
                path: path!["storage", "session"],
                kind: ReportKind::ExplicitJemallocHeapSize,
                size: self.session_data.size_of(ops)
                    + self.session_environments.size_of(ops)
                    + self.session_storage_origins.size_of(ops),
            });
        });
        reports
    }

    fn save_local_storage_origins(&self) {
        if let Some(ref config_dir) = self.config_dir {
            write_json_to_file(&self.local_storage_origins, config_dir, "localstorage.json");
        }
    }

    fn get_origin_location(
        &self,
        storage_type: WebStorageType,
        webview_id: Option<WebViewId>,
        origin: &ImmutableOrigin,
    ) -> Option<PathBuf> {
        match &self.config_dir {
            Some(config_dir) => {
                const NAMESPACE_SERVO_WEBSTORAGE: &uuid::Uuid = &Uuid::from_bytes([
                    0x37, 0x9e, 0x56, 0xb0, 0x1a, 0x76, 0x44, 0xc5, 0xa4, 0xdb, 0xe2, 0x18, 0xc5,
                    0xc8, 0xa3, 0x5d,
                ]);
                let scope = match storage_type {
                    WebStorageType::Local => origin.ascii_serialization(),
                    WebStorageType::Session => format!(
                        "{}|{}",
                        webview_id.expect("session storage requires a webview"),
                        origin.ascii_serialization()
                    ),
                };
                let origin_uuid = Uuid::new_v5(NAMESPACE_SERVO_WEBSTORAGE, scope.as_bytes());
                let base = config_dir.join("webstorage");
                Some(match storage_type {
                    WebStorageType::Local => base.join(origin_uuid.to_string()),
                    WebStorageType::Session => base.join("session").join(origin_uuid.to_string()),
                })
            },
            None => None,
        }
    }

    fn add_new_environment(&mut self, origin: &ImmutableOrigin) -> Result<(), String> {
        let origin_location = self.get_origin_location(WebStorageType::Local, None, origin);

        let engine =
            self.engine_factory
                .open(WebStorageType::Local, None, origin, origin_location)?;
        let environment = WebStorageEnvironment::new(engine);
        self.environments.insert(origin.clone(), environment);
        Ok(())
    }

    fn get_environment(
        &mut self,
        origin: &ImmutableOrigin,
    ) -> Result<&WebStorageEnvironment, String> {
        if self.environments.contains_key(origin) {
            return Ok(self
                .environments
                .get(origin)
                .expect("environment should exist after contains_key check"));
        }

        self.add_new_environment(origin)?;

        Ok(self
            .environments
            .get(origin)
            .expect("environment should exist after add_new_environment"))
    }

    fn get_environment_mut(
        &mut self,
        origin: &ImmutableOrigin,
    ) -> Result<&mut WebStorageEnvironment, String> {
        if self.environments.contains_key(origin) {
            return Ok(self
                .environments
                .get_mut(origin)
                .expect("environment should exist after contains_key check"));
        }

        self.add_new_environment(origin)?;

        Ok(self
            .environments
            .get_mut(origin)
            .expect("environment should exist after add_new_environment"))
    }

    fn add_new_session_environment(
        &mut self,
        webview_id: WebViewId,
        origin: &ImmutableOrigin,
    ) -> Result<(), String> {
        let origin_location =
            self.get_origin_location(WebStorageType::Session, Some(webview_id), origin);
        let engine = self.engine_factory.open(
            WebStorageType::Session,
            Some(webview_id),
            origin,
            origin_location,
        )?;
        self.session_environments
            .entry(webview_id)
            .or_default()
            .insert(origin.clone(), WebStorageEnvironment::new(engine));
        Ok(())
    }

    fn get_session_environment(
        &mut self,
        webview_id: WebViewId,
        origin: &ImmutableOrigin,
    ) -> Result<&WebStorageEnvironment, String> {
        let exists = self
            .session_environments
            .get(&webview_id)
            .is_some_and(|origins| origins.contains_key(origin));
        if !exists {
            self.add_new_session_environment(webview_id, origin)?;
        }
        Ok(self
            .session_environments
            .get(&webview_id)
            .and_then(|origins| origins.get(origin))
            .expect("session environment should exist after add_new_session_environment"))
    }

    fn get_session_environment_mut(
        &mut self,
        webview_id: WebViewId,
        origin: &ImmutableOrigin,
    ) -> Result<&mut WebStorageEnvironment, String> {
        let exists = self
            .session_environments
            .get(&webview_id)
            .is_some_and(|origins| origins.contains_key(origin));
        if !exists {
            self.add_new_session_environment(webview_id, origin)?;
        }
        Ok(self
            .session_environments
            .get_mut(&webview_id)
            .and_then(|origins| origins.get_mut(origin))
            .expect("session environment should exist after add_new_session_environment"))
    }

    fn select_session_data(
        &self,
        webview_id: WebViewId,
        origin: &ImmutableOrigin,
    ) -> Option<&OriginEntry> {
        self.session_data
            .get(&webview_id)
            .and_then(|origin_map| origin_map.get(origin))
    }

    fn select_session_data_mut(
        &mut self,
        webview_id: WebViewId,
        origin: &ImmutableOrigin,
    ) -> Option<&mut OriginEntry> {
        self.session_data
            .get_mut(&webview_id)
            .and_then(|origin_map| origin_map.get_mut(origin))
    }

    fn ensure_session_data_mut(
        &mut self,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
    ) -> &mut OriginEntry {
        self.session_storage_origins
            .ensure_origin_descriptor(&origin);
        self.session_data
            .entry(webview_id)
            .or_default()
            .entry(origin)
            .or_default()
    }

    fn ensure_local_origin(&mut self, origin: &ImmutableOrigin) {
        if self.local_storage_origins.ensure_origin_descriptor(origin) {
            self.save_local_storage_origins();
        }
    }

    fn set_engine_item(
        environment: &mut WebStorageEnvironment,
        name: &str,
        value: &str,
    ) -> Result<Result<(bool, Option<String>), ()>, String> {
        let old_value = environment.engine.get(name)?;
        let total_size = environment.engine.size()?;
        let new_total_size = old_value
            .as_ref()
            .map_or(total_size + name.len() + value.len(), |old| {
                total_size - old.len() + value.len()
            });
        if new_total_size > QUOTA_SIZE_LIMIT {
            return Ok(Err(()));
        }
        if old_value.as_deref() == Some(value) {
            return Ok(Ok((false, None)));
        }
        environment.engine.set(name, value)?;
        Ok(Ok((true, old_value)))
    }

    fn length(
        &mut self,
        sender: GenericSender<usize>,
        storage_type: WebStorageType,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
    ) {
        let length = match storage_type {
            WebStorageType::Session if self.use_engine_for_session => self
                .get_session_environment(webview_id, &origin)
                .and_then(|environment| environment.engine.len())
                .unwrap_or_else(|error| {
                    warn!("Failed to read session Web Storage length: {error}");
                    0
                }),
            WebStorageType::Session => self
                .select_session_data(webview_id, &origin)
                .map_or(0, |entry| entry.inner().len()),
            WebStorageType::Local => {
                self.ensure_local_origin(&origin);
                self.get_environment(&origin)
                    .and_then(|environment| environment.engine.len())
                    .unwrap_or_else(|error| {
                        warn!("Failed to read Web Storage length: {error}");
                        0
                    })
            },
        };
        sender.send(length).unwrap();
    }

    fn key(
        &mut self,
        sender: GenericSender<Option<String>>,
        storage_type: WebStorageType,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
        index: u32,
    ) {
        let key = match storage_type {
            WebStorageType::Session if self.use_engine_for_session => self
                .get_session_environment(webview_id, &origin)
                .and_then(|environment| environment.engine.key(index as usize))
                .unwrap_or_else(|error| {
                    warn!("Failed to read session Web Storage key: {error}");
                    None
                }),
            WebStorageType::Session => self
                .select_session_data(webview_id, &origin)
                .and_then(|entry| entry.inner().keys().nth(index as usize))
                .cloned(),
            WebStorageType::Local => {
                self.ensure_local_origin(&origin);
                self.get_environment(&origin)
                    .and_then(|environment| environment.engine.key(index as usize))
                    .unwrap_or_else(|error| {
                        warn!("Failed to read Web Storage key: {error}");
                        None
                    })
            },
        };
        sender.send(key).unwrap();
    }

    fn keys(
        &mut self,
        sender: GenericSender<Vec<String>>,
        storage_type: WebStorageType,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
    ) {
        let keys = match storage_type {
            WebStorageType::Session if self.use_engine_for_session => self
                .get_session_environment(webview_id, &origin)
                .and_then(|environment| environment.engine.keys())
                .unwrap_or_else(|error| {
                    warn!("Failed to read session Web Storage keys: {error}");
                    vec![]
                }),
            WebStorageType::Session => self
                .select_session_data(webview_id, &origin)
                .map_or(vec![], |entry| entry.inner().keys().cloned().collect()),
            WebStorageType::Local => {
                self.ensure_local_origin(&origin);
                self.get_environment(&origin)
                    .and_then(|environment| environment.engine.keys())
                    .unwrap_or_else(|error| {
                        warn!("Failed to read Web Storage keys: {error}");
                        vec![]
                    })
            },
        };

        sender.send(keys).unwrap();
    }

    /// Sends Ok(changed, Some(old_value)) in case there was a previous
    /// value with the same key name but with different value name
    /// otherwise sends Err(()) to indicate that the operation would result in
    /// exceeding the quota limit
    fn set_item(
        &mut self,
        sender: GenericSender<Result<(bool, Option<String>), ()>>,
        storage_type: WebStorageType,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
        name: String,
        value: String,
    ) {
        let message = match storage_type {
            WebStorageType::Session if self.use_engine_for_session => {
                self.session_storage_origins
                    .ensure_origin_descriptor(&origin);
                let result = self
                    .get_session_environment_mut(webview_id, &origin)
                    .and_then(|environment| Self::set_engine_item(environment, &name, &value));
                result.unwrap_or_else(|error| {
                    warn!("Failed to set session Web Storage item: {error}");
                    Err(())
                })
            },
            WebStorageType::Session => {
                let entry = self.ensure_session_data_mut(webview_id, origin);
                let total_size = entry.size();
                let old_value = entry.inner().get(&name);
                let new_total_size = old_value
                    .map_or(total_size + name.len() + value.len(), |old| {
                        total_size - old.len() + value.len()
                    });
                if new_total_size > QUOTA_SIZE_LIMIT {
                    Err(())
                } else {
                    entry
                        .insert(name, value.clone())
                        .map_or(Ok((true, None)), |old| {
                            if old == value {
                                Ok((false, None))
                            } else {
                                Ok((true, Some(old)))
                            }
                        })
                }
            },
            WebStorageType::Local => {
                self.ensure_local_origin(&origin);
                let result = self
                    .get_environment_mut(&origin)
                    .and_then(|environment| Self::set_engine_item(environment, &name, &value));
                result.unwrap_or_else(|error| {
                    warn!("Failed to set Web Storage item: {error}");
                    Err(())
                })
            },
        };
        sender.send(message).unwrap();
    }

    fn request_item(
        &mut self,
        sender: GenericSender<Option<String>>,
        storage_type: WebStorageType,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
        name: String,
    ) {
        let value = match storage_type {
            WebStorageType::Session if self.use_engine_for_session => self
                .get_session_environment(webview_id, &origin)
                .and_then(|environment| environment.engine.get(&name))
                .unwrap_or_else(|error| {
                    warn!("Failed to get session Web Storage item: {error}");
                    None
                }),
            WebStorageType::Session => self
                .select_session_data(webview_id, &origin)
                .and_then(|entry| entry.inner().get(&name))
                .cloned(),
            WebStorageType::Local => {
                self.ensure_local_origin(&origin);
                self.get_environment(&origin)
                    .and_then(|environment| environment.engine.get(&name))
                    .unwrap_or_else(|error| {
                        warn!("Failed to get Web Storage item: {error}");
                        None
                    })
            },
        };
        sender.send(value).unwrap();
    }

    /// Sends Some(old_value) in case there was a previous value with the key name, otherwise sends None
    fn remove_item(
        &mut self,
        sender: GenericSender<Option<String>>,
        storage_type: WebStorageType,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
        name: String,
    ) {
        let old_value = match storage_type {
            WebStorageType::Session if self.use_engine_for_session => self
                .get_session_environment_mut(webview_id, &origin)
                .and_then(|environment| environment.engine.delete(&name))
                .unwrap_or_else(|error| {
                    warn!("Failed to remove session Web Storage item: {error}");
                    None
                }),
            WebStorageType::Session => self
                .select_session_data_mut(webview_id, &origin)
                .and_then(|entry| entry.remove(&name)),
            WebStorageType::Local => {
                self.ensure_local_origin(&origin);
                self.get_environment_mut(&origin)
                    .and_then(|environment| environment.engine.delete(&name))
                    .unwrap_or_else(|error| {
                        warn!("Failed to remove Web Storage item: {error}");
                        None
                    })
            },
        };
        sender.send(old_value).unwrap();
    }

    fn clear(
        &mut self,
        sender: GenericSender<bool>,
        storage_type: WebStorageType,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
    ) {
        let changed = match storage_type {
            WebStorageType::Session if self.use_engine_for_session => self
                .get_session_environment_mut(webview_id, &origin)
                .and_then(|environment| environment.engine.clear())
                .unwrap_or_else(|error| {
                    warn!("Failed to clear session Web Storage: {error}");
                    false
                }),
            WebStorageType::Session => self
                .select_session_data_mut(webview_id, &origin)
                .is_some_and(|entry| {
                    if !entry.inner().is_empty() {
                        entry.clear();
                        true
                    } else {
                        false
                    }
                }),
            WebStorageType::Local => {
                self.ensure_local_origin(&origin);
                self.get_environment_mut(&origin)
                    .and_then(|environment| environment.engine.clear())
                    .unwrap_or_else(|error| {
                        warn!("Failed to clear Web Storage: {error}");
                        false
                    })
            },
        };
        sender.send(changed).unwrap();
    }

    fn clone(&mut self, src_webview_id: WebViewId, dest_webview_id: WebViewId) {
        if self.use_engine_for_session {
            if let Some(dest_origins) = self.session_environments.remove(&dest_webview_id) {
                for (_, mut environment) in dest_origins {
                    if let Err(error) = environment.engine.clear() {
                        warn!("Failed to clear destination session Web Storage: {error}");
                    }
                }
            }

            let snapshots = self
                .session_environments
                .get(&src_webview_id)
                .map(|origins| {
                    origins
                        .iter()
                        .filter_map(|(origin, environment)| {
                            let snapshot = environment.engine.keys().and_then(|keys| {
                                keys.into_iter()
                                    .map(|key| {
                                        environment
                                            .engine
                                            .get(&key)
                                            .map(|value| value.map(|value| (key, value)))
                                    })
                                    .collect::<Result<Option<Vec<_>>, _>>()
                            });
                            match snapshot {
                                Ok(Some(entries)) => Some((origin.clone(), entries)),
                                Ok(None) => {
                                    warn!("Session Web Storage key disappeared during clone");
                                    None
                                },
                                Err(error) => {
                                    warn!("Failed to read session Web Storage for clone: {error}");
                                    None
                                },
                            }
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            for (origin, entries) in snapshots {
                let result = self
                    .get_session_environment_mut(dest_webview_id, &origin)
                    .and_then(|environment| {
                        environment.engine.clear()?;
                        for (key, value) in entries {
                            environment.engine.set(&key, &value)?;
                        }
                        Ok(())
                    });
                if let Err(error) = result {
                    warn!("Failed to clone session Web Storage: {error}");
                }
            }
            return;
        }

        let Some(src_origin_entries) = self.session_data.get(&src_webview_id) else {
            return;
        };

        let dest_origin_entries = src_origin_entries.clone();
        self.session_data
            .insert(dest_webview_id, dest_origin_entries);
    }

    fn origin_descriptors(&mut self, storage_type: WebStorageType) -> Vec<OriginDescriptor> {
        match storage_type {
            WebStorageType::Session => self.session_storage_origins.origin_descriptors(),
            WebStorageType::Local => self.local_storage_origins.origin_descriptors(),
        }
    }

    fn clear_data_for_sites(&mut self, storage_type: WebStorageType, sites: &[String]) {
        match storage_type {
            WebStorageType::Session => {
                let origins = self.session_storage_origins.take_origins_for_sites(sites);

                if self.use_engine_for_session {
                    let mut failed_origins = Vec::new();
                    self.session_environments.retain(|_, origins_map| {
                        for origin in &origins {
                            if let Some(mut environment) = origins_map.remove(origin) {
                                if let Err(error) = environment.engine.clear() {
                                    warn!("Failed to clear session Web Storage origin: {error}");
                                    origins_map.insert(origin.clone(), environment);
                                    failed_origins.push(origin.clone());
                                }
                            }
                        }
                        !origins_map.is_empty()
                    });
                    for origin in failed_origins {
                        self.session_storage_origins
                            .ensure_origin_descriptor(&origin);
                    }
                    return;
                }

                self.session_data.retain(|_, origins_map| {
                    for origin in &origins {
                        origins_map.remove(origin);
                    }
                    !origins_map.is_empty()
                });
            },
            WebStorageType::Local => {
                let origins = self.local_storage_origins.take_origins_for_sites(sites);

                for origin in origins {
                    let clear_result = self
                        .get_environment_mut(&origin)
                        .and_then(|environment| environment.engine.clear());
                    if let Err(error) = clear_result {
                        warn!("Failed to clear local Web Storage origin: {error}");
                        self.local_storage_origins.ensure_origin_descriptor(&origin);
                        continue;
                    }

                    self.environments.remove(&origin);
                    if self.config_dir.is_some() {
                        let origin_location = self
                            .get_origin_location(WebStorageType::Local, None, &origin)
                            .expect("Should always be able to get origin location.");
                        if let Err(error) = std::fs::remove_dir_all(&origin_location) {
                            if error.kind() != std::io::ErrorKind::NotFound {
                                warn!("Failed to delete origin location: {:?}", error);
                                self.local_storage_origins.ensure_origin_descriptor(&origin);
                            }
                        }
                    }
                }

                self.save_local_storage_origins();
            },
        }
    }
}
