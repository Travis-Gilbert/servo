/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::BTreeMap;
use std::path::PathBuf;

use malloc_size_of_derive::MallocSizeOf;
use profile_traits::mem::ReportsChan;
use serde::{Deserialize, Serialize};
use servo_base::generic_channel::GenericSender;
use servo_base::id::WebViewId;
use servo_url::ImmutableOrigin;

#[derive(Clone, Default, MallocSizeOf)]
pub struct OriginEntry {
    tree: BTreeMap<String, String>,
    size: usize,
}

impl OriginEntry {
    pub fn inner(&self) -> &BTreeMap<String, String> {
        &self.tree
    }

    pub fn insert(&mut self, key: String, value: String) -> Option<String> {
        let old_value = self.tree.insert(key.clone(), value.clone());
        let size_change = match &old_value {
            Some(old) => value.len() as isize - old.len() as isize,
            None => (key.len() + value.len()) as isize,
        };
        self.size = (self.size as isize + size_change) as usize;
        old_value
    }

    pub fn remove(&mut self, key: &str) -> Option<String> {
        let old_value = self.tree.remove(key);
        if let Some(old) = &old_value {
            self.size -= key.len() + old.len();
        }
        old_value
    }

    pub fn clear(&mut self) {
        self.tree.clear();
        self.size = 0;
    }

    pub fn size(&self) -> usize {
        self.size
    }
}

pub trait WebStorageEngine: Send {
    fn len(&self) -> Result<usize, String>;
    fn key(&self, index: usize) -> Result<Option<String>, String>;
    fn keys(&self) -> Result<Vec<String>, String>;
    fn get(&self, key: &str) -> Result<Option<String>, String>;
    fn set(&mut self, key: &str, value: &str) -> Result<Option<String>, String>;
    fn delete(&mut self, key: &str) -> Result<Option<String>, String>;
    fn clear(&mut self) -> Result<bool, String>;
    fn size(&self) -> Result<usize, String>;
}

pub trait WebStorageEngineFactory: Send + Sync {
    fn open(
        &self,
        storage_type: WebStorageType,
        webview_id: Option<WebViewId>,
        origin: &ImmutableOrigin,
        db_dir: Option<PathBuf>,
    ) -> Result<Box<dyn WebStorageEngine>, String>;
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, MallocSizeOf, Serialize)]
pub enum WebStorageType {
    Session,
    Local,
}

#[derive(Clone, Debug, Deserialize, MallocSizeOf, Serialize)]
pub struct OriginDescriptor {
    pub name: String,
}

impl OriginDescriptor {
    pub fn new(name: String) -> Self {
        OriginDescriptor { name }
    }
}

/// Request operations on the storage data associated with a particular url
#[derive(Debug, Deserialize, Serialize)]
pub enum WebStorageThreadMsg {
    /// gets the number of key/value pairs present in the associated storage data
    Length(
        GenericSender<usize>,
        WebStorageType,
        WebViewId,
        ImmutableOrigin,
    ),

    /// gets the name of the key at the specified index in the associated storage data
    Key(
        GenericSender<Option<String>>,
        WebStorageType,
        WebViewId,
        ImmutableOrigin,
        u32,
    ),

    /// Gets the available keys in the associated storage data
    Keys(
        GenericSender<Vec<String>>,
        WebStorageType,
        WebViewId,
        ImmutableOrigin,
    ),

    /// gets the value associated with the given key in the associated storage data
    GetItem(
        GenericSender<Option<String>>,
        WebStorageType,
        WebViewId,
        ImmutableOrigin,
        String,
    ),

    /// sets the value of the given key in the associated storage data
    SetItem(
        GenericSender<Result<(bool, Option<String>), ()>>,
        WebStorageType,
        WebViewId,
        ImmutableOrigin,
        String,
        String,
    ),

    /// removes the key/value pair for the given key in the associated storage data
    RemoveItem(
        GenericSender<Option<String>>,
        WebStorageType,
        WebViewId,
        ImmutableOrigin,
        String,
    ),

    /// clears the associated storage data by removing all the key/value pairs
    Clear(
        GenericSender<bool>,
        WebStorageType,
        WebViewId,
        ImmutableOrigin,
    ),

    /// clones all storage data of the given top-level browsing context for a new browsing context.
    /// should only be used for sessionStorage.
    Clone {
        sender: GenericSender<()>,
        src: WebViewId,
        dest: WebViewId,
    },

    /// gets the list of origin descriptors for given storage type
    ///
    /// TODO: Consider returning `Vec<SiteDescriptor>`
    ListOrigins(GenericSender<Vec<OriginDescriptor>>, WebStorageType),

    /// clears storage data for given storage type and sites, affecting all matching origins
    ClearDataForSites(GenericSender<()>, WebStorageType, Vec<String>),

    /// send a reply when done cleaning up thread resources and then shut it down
    Exit(GenericSender<()>),

    /// Measure memory used by this thread and send the report over the provided channel.
    CollectMemoryReport(ReportsChan),
}
