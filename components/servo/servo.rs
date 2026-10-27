/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, Ref, RefCell, RefMut};
use std::cmp::max;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;

use accesskit::ActionRequest;
use crossbeam_channel::{Receiver, Sender, unbounded};
pub use embedder_traits::*;
use env_logger::Builder as EnvLoggerBuilder;
use fonts::SystemFontService;
#[cfg(all(
    feature = "multiprocess",
    not(target_os = "windows"),
    not(target_os = "ios"),
    not(target_os = "android"),
    not(target_arch = "arm"),
    not(target_arch = "aarch64"),
    not(target_arch = "riscv32"),
    not(target_arch = "riscv64"),
    not(target_env = "ohos"),
))]
use gaol::sandbox::{ChildSandbox, ChildSandboxMethods};
#[cfg(feature = "multiprocess")]
use ipc_channel::ipc::{self, IpcSender};
use layout::LayoutFactoryImpl;
#[cfg(feature = "multiprocess")]
use layout_api::ScriptThreadFactory;
use log::{Log, Metadata, Record, debug, warn};
use media::{GlApi, NativeDisplay, WindowGLContext};
use net::connector::CACertificates;
use net::embedder::NetToEmbedderMsg;
#[cfg(feature = "multiprocess")]
use net::image_cache::ImageCacheFactoryImpl;
use net::protocols::ProtocolRegistry;
use net::resource_thread::{load_root_cert_store_from_file, new_partition_resource_threads};
#[cfg(feature = "multiprocess")]
use net_traits::FetchThread;
use net_traits::ResourceThreads;
use paint::{InitialPaintState, Paint};
pub use paint_api::rendering_context::RenderingContext;
use paint_api::{CrossProcessPaintApi, PaintMessage, PaintProxy};
use profile::{mem as profile_mem, time as profile_time};
use profile_traits::mem::MemoryReportResult;
#[cfg(feature = "multiprocess")]
use profile_traits::mem::{ProfilerMsg, Reporter};
use profile_traits::{mem, time};
use rustc_hash::FxHashMap;
use script::JSEngineSetup;
#[cfg(feature = "multiprocess")]
use script::ServiceWorkerManager;
#[cfg(feature = "multiprocess")]
use servo_background_hang_monitor::HangMonitorRegister;
#[cfg(feature = "bluetooth")]
use servo_base::generic_channel::GenericSender;
use servo_base::generic_channel::{GenericCallback, RoutedReceiver, SendError};
pub use servo_base::id::WebViewId;
use servo_base::id::{EMBEDDER_PIPELINE_NAMESPACE_ID, PipelineNamespace, StoragePartitionId};
#[cfg(feature = "bluetooth")]
use servo_bluetooth::BluetoothThreadFactory;
#[cfg(feature = "bluetooth")]
use servo_bluetooth_traits::BluetoothRequest;
use servo_config::opts::{DiagnosticsLoggingOption, Opts};
use servo_config::prefs::{PrefValue, Preferences};
use servo_config::{opts, pref, prefs};
#[cfg(feature = "multiprocess")]
use servo_constellation::UnprivilegedContent;
#[cfg(all(
    feature = "multiprocess",
    not(target_os = "windows"),
    not(target_os = "ios"),
    not(target_os = "android"),
    not(target_arch = "arm"),
    not(target_arch = "aarch64"),
    not(target_arch = "riscv32"),
    not(target_arch = "riscv64"),
    not(target_env = "ohos"),
))]
use servo_constellation::content_process_sandbox_profile;
use servo_constellation::{
    Constellation, ConstellationToEmbedderMsg, FromEmbedderLogger, InitialConstellationState,
};
#[cfg(feature = "multiprocess")]
use servo_constellation::{FromScriptLogger, NewScriptEventLoopProcessInfo};
use servo_constellation_traits::EmbedderToConstellationMessage;
#[cfg(feature = "multiprocess")]
use servo_constellation_traits::ScriptToConstellationSender;
use servo_geometry::{
    DeviceIndependentIntRect, convert_rect_to_css_pixel, convert_size_to_css_pixel,
};
use servo_media::ServoMedia;
use servo_media::player::context::GlContext;
use servo_wakelock::DefaultWakeLockDelegate;
use storage::new_partition_storage_threads;
use storage_traits::{StorageEngines, StorageThreads};
#[cfg(feature = "multiprocess")]
use style::global_style_data::StyleThreadPool;
#[cfg(feature = "webxr")]
use webxr::WebXrRegistry;

use crate::clipboard_delegate::StringRequest;
#[cfg(feature = "gamepad")]
use crate::gamepad_delegate::{GamepadHapticEffectRequest, GamepadHapticEffectRequestType};
use crate::javascript_evaluator::JavaScriptEvaluator;
use crate::network_manager::NetworkManager;
use crate::proxies::ConstellationProxy;
use crate::responders::ServoErrorChannel;
use crate::servo_delegate::{DefaultServoDelegate, ServoDelegate, ServoError};
use crate::site_data_manager::{CookieCallbacks, CookieOperationResponse, SiteDataManager};
use crate::webview::{MINIMUM_WEBVIEW_SIZE, WebView, WebViewInner};
use crate::webview_delegate::{
    AllowOrDenyRequest, AuthenticationRequest, BluetoothDeviceSelectionRequest, EmbedderControl,
    FilePicker, NavigationRequest, PermissionRequest, ProtocolHandlerRegistration, WebResourceLoad,
};

#[cfg(feature = "media-gstreamer")]
mod media_platform {
    use servo_media_gstreamer::GStreamerBackend;

    use super::ServoMedia;

    #[cfg(any(windows, target_os = "macos"))]
    pub fn init() {
        ServoMedia::init_with_backend(|| {
            let mut plugin_dir = std::env::current_exe().unwrap();
            plugin_dir.pop();

            if cfg!(target_os = "macos") {
                plugin_dir.push("lib");
            }

            let plugin_list = crate::gstreamer_plugins::gstreamer_plugins();
            match GStreamerBackend::init_with_plugins(plugin_dir, &plugin_list) {
                Ok(b) => b,
                Err(e) => {
                    log::error!("Error initializing GStreamer: {:?}", e);
                    std::process::exit(1);
                },
            }
        });
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    pub fn init() {
        ServoMedia::init::<GStreamerBackend>();
    }
}

#[cfg(all(not(feature = "media-gstreamer"), target_env = "ohos"))]
mod media_platform {
    use servo_media_ohos::OhosBackend;

    use super::ServoMedia;
    pub fn init() {
        ServoMedia::init::<OhosBackend>();
    }
}

#[cfg(all(not(feature = "media-gstreamer"), not(target_env = "ohos")))]
mod media_platform {
    use super::ServoMedia;
    pub fn init() {
        ServoMedia::init::<servo_media_dummy::DummyBackend>();
    }
}

#[allow(clippy::enum_variant_names)]
enum Message {
    FromNet(NetToEmbedderMsg),
    FromConstellation(ConstellationToEmbedderMsg),
    FromUnknown(EmbedderMsg),
}

/// Holds a prebuilt `crossbeam_channel::Select` over the crossbeam
/// receivers so our [`spin_event_loop`] loop doesn't rebuild it on every
/// iteration.
///
/// [`spin_event_loop`]: ServoInner::spin_event_loop
struct EmbedderMessageSelector<'a> {
    select: crossbeam_channel::Select<'a>,
    receivers: (
        &'a Receiver<EmbedderMsg>,
        &'a Receiver<NetToEmbedderMsg>,
        &'a Receiver<ConstellationToEmbedderMsg>,
    ),
}

impl<'a> EmbedderMessageSelector<'a> {
    fn new(
        embedder_receiver: &'a Receiver<EmbedderMsg>,
        net_embedder_receiver: &'a Receiver<NetToEmbedderMsg>,
        constellation_embedder_receiver: &'a Receiver<ConstellationToEmbedderMsg>,
    ) -> Self {
        let mut select = crossbeam_channel::Select::new();
        // The order of `.recv()` calls **must** match the order of fields in `receivers`.
        // In `try_recv()` we assume the `select` index matches the order of receivers in the tuple.
        let embedder_index = select.recv(embedder_receiver);
        debug_assert_eq!(embedder_index, 0);
        let net_embedder_index = select.recv(net_embedder_receiver);
        debug_assert_eq!(net_embedder_index, 1);
        let constellation_embedder_index = select.recv(constellation_embedder_receiver);
        debug_assert_eq!(constellation_embedder_index, 2);
        Self {
            select,
            receivers: (
                embedder_receiver,
                net_embedder_receiver,
                constellation_embedder_receiver,
            ),
        }
    }

    #[servo_tracing::instrument(
        level = "debug",
        name = "EmbedderMessageSelector::try_recv_one_message",
        skip_all
    )]
    fn try_recv_one_message(&mut self) -> Option<Message> {
        let operation = self.select.try_select().ok()?;
        let index = operation.index();
        if index == 0 {
            let message = operation.recv(self.receivers.0).ok()?;
            Some(Message::FromUnknown(message))
        } else if index == 1 {
            let message = operation.recv(self.receivers.1).ok()?;
            Some(Message::FromNet(message))
        } else if index == 2 {
            let message = operation.recv(self.receivers.2).ok()?;
            Some(Message::FromConstellation(message))
        } else {
            log::error!("No select operation registered for {index:?}");
            None
        }
    }
}

pub struct PendingHandledInputEvent {
    pub event_id: InputEventId,
    pub webview_id: WebViewId,
}

/// A Servo-owned storage partition. Cloning this handle reuses its owners.
/// Dropping a handle does not unload or delete the partition.
#[derive(Clone)]
pub struct StoragePartition(Rc<StoragePartitionInner>);

struct StoragePartitionInner {
    id: StoragePartitionId,
    root: Option<PathBuf>,
    resources: ResourceThreads,
    storage: StorageThreads,
    site_data: SiteDataManager,
    network: NetworkManager,
    // Kept by Servo until all partition owners have stopped.
    _temporary_root: RefCell<Option<tempfile::TempDir>>,
}

impl std::fmt::Debug for StoragePartition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoragePartition")
            .field("id", &self.id())
            .field("root", &self.root())
            .finish_non_exhaustive()
    }
}

impl StoragePartition {
    /// The immutable identity carried by this partition's owner channels.
    pub fn id(&self) -> StoragePartitionId {
        self.0.id
    }
    /// The canonical persistent root, or `None` for an ephemeral partition.
    pub fn root(&self) -> Option<&Path> {
        self.0.root.as_deref()
    }
    /// Manage site data belonging to this partition only.
    pub fn site_data_manager(&self) -> &SiteDataManager {
        &self.0.site_data
    }
    /// Manage the HTTP cache belonging to this partition only.
    pub fn network_manager(&self) -> &NetworkManager {
        &self.0.network
    }
}

/// Partition creation or WebView selection failed without selecting another owner.
#[derive(Debug)]
pub enum StoragePartitionError {
    Io(std::io::Error),
    DuplicateRoot(PathBuf),
    Registration(String),
    WrongServo,
    PopupPartitionConflict,
    ShuttingDown,
}

impl std::fmt::Display for StoragePartitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "Partition directory: {error}"),
            Self::DuplicateRoot(root) => {
                write!(f, "Partition root already registered: {}", root.display())
            },
            Self::Registration(error) => write!(f, "Partition registration: {error}"),
            Self::WrongServo => f.write_str("Partition does not belong to this Servo"),
            Self::PopupPartitionConflict => {
                f.write_str("An auxiliary WebView must inherit its opener's partition")
            },
            Self::ShuttingDown => f.write_str("Servo is shutting down"),
        }
    }
}
impl std::error::Error for StoragePartitionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}
impl From<std::io::Error> for StoragePartitionError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

struct PartitionFactory {
    devtools_sender: Option<Sender<devtools_traits::DevtoolsControlMsg>>,
    time_profiler: time::ProfilerChan,
    mem_profiler: mem::ProfilerChan,
    embedder_proxy: GenericEmbedderProxy<NetToEmbedderMsg>,
    certificates: CACertificates<'static>,
    ignore_certificate_errors: bool,
    protocols: Arc<ProtocolRegistry>,
    engines: StorageEngines,
    cookie_callbacks: Rc<CookieCallbacks>,
}

impl PartitionFactory {
    fn create(
        &self,
        id: StoragePartitionId,
        root: Option<PathBuf>,
    ) -> Result<StoragePartition, StoragePartitionError> {
        let root = root
            .map(|root| {
                std::fs::create_dir_all(&root)?;
                std::fs::canonicalize(root)
            })
            .transpose()?;
        let temporary_root = if root.is_none() {
            Some(tempfile::tempdir()?)
        } else {
            None
        };
        let storage_root = root
            .clone()
            .or_else(|| temporary_root.as_ref().map(|dir| dir.path().to_owned()));
        let disk_cache_path = if !pref!(network_http_disk_cache).is_empty() {
            root.as_ref()
                .map(|root| root.join("http-cache/cache.sqlite3"))
        } else {
            None
        };
        if let Some(path) = &disk_cache_path {
            std::fs::create_dir_all(path.parent().unwrap())?;
        }
        // Fail directory errors before starting any owner threads.
        if let Some(root) = &storage_root {
            for directory in [
                "clientstorage/default_v1",
                "cachestorage/default_v1",
                "webstorage",
            ] {
                std::fs::create_dir_all(root.join(directory))?;
            }
        }
        let resources = new_partition_resource_threads(
            id,
            self.devtools_sender.clone(),
            self.time_profiler.clone(),
            self.mem_profiler.clone(),
            self.embedder_proxy.clone(),
            root.clone(),
            disk_cache_path,
            self.certificates.clone(),
            self.ignore_certificate_errors,
            self.protocols.clone(),
        );
        let storage = new_partition_storage_threads(
            id,
            self.mem_profiler.clone(),
            storage_root,
            root.is_none(),
            &format!("{id:?}"),
            &self.engines,
        );
        let (sender, receiver) = servo_base::generic_channel::channel().unwrap();
        let network_ready = resources
            .core_thread
            .send(net_traits::CoreResourceMsg::ListCookies(sender))
            .map_err(|e| e.to_string())
            .and_then(|_| receiver.recv().map(|_| ()).map_err(|e| e.to_string()));
        if let Err(error) = network_ready.and_then(|_| storage.wait_until_ready()) {
            let _ = storage.exit();
            let (sender, receiver) = servo_base::generic_channel::oneshot().unwrap();
            if resources
                .core_thread
                .send(net_traits::CoreResourceMsg::Exit(sender))
                .is_ok()
            {
                let _ = receiver.recv();
            }
            return Err(StoragePartitionError::Registration(error));
        }
        Ok(StoragePartition(Rc::new(StoragePartitionInner {
            id,
            root,
            site_data: SiteDataManager::new(
                resources.clone(),
                storage.clone(),
                self.cookie_callbacks.clone(),
            ),
            network: NetworkManager::new(resources.clone()),
            resources,
            storage,
            _temporary_root: RefCell::new(temporary_root),
        })))
    }
}

struct ServoInner {
    partitions: RefCell<FxHashMap<StoragePartitionId, StoragePartition>>,
    partition_factory: PartitionFactory,
    default_partition: StoragePartition,
    delegate: RefCell<Rc<dyn ServoDelegate>>,
    paint: Rc<RefCell<Paint>>,
    constellation_proxy: ConstellationProxy,
    embedder_receiver: Receiver<EmbedderMsg>,
    net_embedder_receiver: Receiver<NetToEmbedderMsg>,
    constellation_embedder_receiver: Receiver<ConstellationToEmbedderMsg>,
    network_manager: Rc<RefCell<NetworkManager>>,
    site_data_manager: SiteDataManager,
    /// A struct that tracks ongoing JavaScript evaluations and is responsible for
    /// calling the callback when the evaluation is complete.
    javascript_evaluator: Rc<RefCell<JavaScriptEvaluator>>,
    /// Tracks whether we are in the process of shutting down, or have shut down.
    /// This is shared with `WebView`s and the `ServoRenderer`.
    shutdown_state: Rc<Cell<ShutdownState>>,
    /// A map  [`WebView`]s that are managed by this [`Servo`] instance. These are stored
    /// as `Weak` references so that the embedding application can control their lifetime.
    /// When accessed, `Servo` will be reponsible for cleaning up the invalid `Weak`
    /// references.
    webviews: RefCell<FxHashMap<WebViewId, Weak<RefCell<WebViewInner>>>>,
    servo_errors: ServoErrorChannel,
    /// For single-process Servo instances, this field controls the initialization
    /// and deinitialization of the JS Engine. Multiprocess Servo instances have their
    /// own instance that exists in the content process instead.
    _js_engine_setup: Option<JSEngineSetup>,
    /// [`InputEventId`]s that have been handled, but for which the embedder has
    /// not been notified yet.
    pending_handled_input_events: RefCell<Vec<PendingHandledInputEvent>>,
    /// An [`EventLoopWaker`] used to wake up the main embedder event loop.
    event_loop_waker: Box<dyn EventLoopWaker>,
}

impl ServoInner {
    fn get_webview_handle(&self, id: WebViewId) -> Option<WebView> {
        self.webviews
            .borrow()
            .get(&id)
            .and_then(WebView::from_weak_handle)
    }

    #[servo_tracing::instrument(level = "debug", skip_all)]
    fn spin_event_loop(&self) -> bool {
        if self.shutdown_state.get() == ShutdownState::FinishedShuttingDown {
            return false;
        }

        {
            let paint = self.paint.borrow();
            let mut messages = Vec::new();
            while let Ok(message) = paint.receiver().try_recv() {
                match message {
                    Ok(message) => messages.push(message),
                    Err(error) => {
                        warn!("Router deserialization error: {error}. Ignoring this PaintMessage.")
                    },
                }
            }
            paint.handle_messages(messages);
        }

        let mut selector = EmbedderMessageSelector::new(
            &self.embedder_receiver,
            &self.net_embedder_receiver,
            &self.constellation_embedder_receiver,
        );
        // Only handle incoming embedder messages if `Paint` hasn't already started shutting down.
        while let Some(message) = selector.try_recv_one_message() {
            match message {
                Message::FromUnknown(message) => self.handle_embedder_message(message),
                Message::FromNet(message) => self.handle_net_embedder_message(message),
                Message::FromConstellation(message) => {
                    self.handle_constellation_embedder_message(message)
                },
            }
            if self.shutdown_state.get() == ShutdownState::FinishedShuttingDown {
                break;
            }
        }
        let pending_handled_input_events =
            std::mem::take(&mut *self.pending_handled_input_events.borrow_mut());
        for PendingHandledInputEvent {
            event_id,
            webview_id,
        } in pending_handled_input_events
        {
            self.paint.borrow_mut().notify_input_event_handled(
                webview_id,
                event_id,
                InputEventResult::DispatchFailed,
            );
            if let Some(webview) = self.get_webview_handle(webview_id) {
                webview.delegate().notify_input_event_handled(
                    webview,
                    event_id,
                    InputEventResult::DispatchFailed,
                );
            }
        }

        if self.constellation_proxy.disconnected() {
            self.delegate
                .borrow()
                .notify_error(ServoError::LostConnectionWithBackend);
        }

        self.paint.borrow_mut().perform_updates();
        self.resend_accessibility_root_nodes_for_viewport_changes();
        self.send_new_frame_ready_messages();
        self.handle_delegate_errors();
        self.clean_up_destroyed_webview_handles();

        if self.shutdown_state.get() == ShutdownState::FinishedShuttingDown {
            return false;
        }

        true
    }

    /// Resend the root accessibility node for any [`WebView`] whose viewport geometry changed
    /// post-last-spin (see [`WebView::note_accessibility_viewport_changed()`]). This runs
    /// after `perform_updates` so the paint `RefCell` is no longer borrowed, and our
    /// embedder-facing methods calling into the [`WebViewDelegate`] avoid re-entrant
    /// borrows within the embedder!
    fn resend_accessibility_root_nodes_for_viewport_changes(&self) {
        // Collect handles first so the `webviews` borrow is released before we call into the
        // WebViewDelegate, which the embedder may re-enter.
        let webviews: Vec<WebView> = self
            .webviews
            .borrow()
            .values()
            .filter_map(WebView::from_weak_handle)
            .collect();
        for webview in webviews {
            if webview.take_accessibility_viewport_changed() {
                webview.send_accessibility_root_node();
            }
        }
    }

    fn send_new_frame_ready_messages(&self) {
        let webviews_needing_repaint = self.paint.borrow().webviews_needing_repaint();

        for webview in webviews_needing_repaint
            .iter()
            .filter_map(|webview_id| self.get_webview_handle(*webview_id))
        {
            webview.delegate().notify_new_frame_ready(webview);
        }
    }

    fn handle_delegate_errors(&self) {
        while let Some(error) = self.servo_errors.try_recv() {
            self.delegate.borrow().notify_error(error);
        }
    }

    fn clean_up_destroyed_webview_handles(&self) {
        // Remove any webview handles that have been destroyed and would not be upgradable.
        // Note that `retain` is O(capacity) because it visits empty buckets, so it may be worth
        // calling `shrink_to_fit` at some point to deal with cases where a long-running Servo
        // instance goes from many open webviews to only a few.
        self.webviews
            .borrow_mut()
            .retain(|_webview_id, webview| webview.strong_count() > 0);
    }

    fn finish_shutting_down(&self) {
        debug!("Servo received message that Constellation shutdown is complete");
        self.shutdown_state.set(ShutdownState::FinishedShuttingDown);
        for partition in self.partitions.borrow().values() {
            partition.0._temporary_root.borrow_mut().take();
        }
        self.paint.borrow_mut().finish_shutting_down();
    }

    fn handle_net_embedder_message(&self, message: NetToEmbedderMsg) {
        match message {
            NetToEmbedderMsg::SelectFiles(control_id, file_picker_request, response_sender) => {
                if file_picker_request.accept_current_paths_for_testing {
                    let _ = response_sender.send(Some(file_picker_request.current_paths));
                    return;
                }
                if let Some(webview) = self.get_webview_handle(control_id.webview_id) {
                    webview.delegate().show_embedder_control(
                        webview,
                        EmbedderControl::FilePicker(FilePicker {
                            id: control_id,
                            file_picker_request,
                            response_sender: Some(response_sender),
                        }),
                    );
                }
            },
            NetToEmbedderMsg::WebResourceRequested(
                webview_id,
                web_resource_request,
                response_sender,
            ) => {
                if let Some(webview) =
                    webview_id.and_then(|webview_id| self.get_webview_handle(webview_id))
                {
                    let web_resource_load = WebResourceLoad::new(
                        web_resource_request,
                        response_sender,
                        self.servo_errors.sender(),
                    );
                    webview
                        .delegate()
                        .load_web_resource(webview, web_resource_load);
                } else {
                    let web_resource_load = WebResourceLoad::new(
                        web_resource_request,
                        response_sender,
                        self.servo_errors.sender(),
                    );
                    self.delegate.borrow().load_web_resource(web_resource_load);
                }
            },
            NetToEmbedderMsg::RequestAuthentication(
                webview_id,
                url,
                for_proxy,
                response_sender,
            ) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    let authentication_request = AuthenticationRequest::new(
                        url.into_url(),
                        for_proxy,
                        response_sender,
                        self.servo_errors.sender(),
                    );
                    webview
                        .delegate()
                        .request_authentication(webview, authentication_request);
                }
            },
            NetToEmbedderMsg::EmbedderCookieOperationResponseWithCookies(operation_id, cookies) => {
                self.site_data_manager.handle_cookie_response(
                    operation_id,
                    CookieOperationResponse::Cookies(cookies),
                );
            },
            NetToEmbedderMsg::EmbedderCookieOperationResponse(operation_id) => {
                self.site_data_manager
                    .handle_cookie_response(operation_id, CookieOperationResponse::Done);
            },
        }
    }

    fn handle_embedder_message(&self, message: EmbedderMsg) {
        match message {
            EmbedderMsg::Status(webview_id, status_text) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    webview.set_status_text(status_text);
                }
            },
            EmbedderMsg::ChangePageTitle(webview_id, title) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    webview.set_page_title(title);
                }
            },
            EmbedderMsg::MoveTo(webview_id, position) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    webview.delegate().request_move_to(webview, position);
                }
            },
            EmbedderMsg::ResizeTo(webview_id, size) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    webview
                        .delegate()
                        .request_resize_to(webview, size.max(MINIMUM_WEBVIEW_SIZE));
                }
            },
            EmbedderMsg::ShowSimpleDialog(webview_id, simple_dialog) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    webview.delegate().show_embedder_control(
                        webview,
                        EmbedderControl::SimpleDialog(simple_dialog.into()),
                    );
                }
            },
            EmbedderMsg::AllowProtocolHandlerRequest(
                webview_id,
                registration_update,
                response_sender,
            ) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    let ProtocolHandlerUpdateRegistration {
                        scheme,
                        url,
                        register_or_unregister,
                    } = registration_update;
                    let protocol_handler_registration = ProtocolHandlerRegistration {
                        scheme,
                        url: url.into_url(),
                        register_or_unregister,
                    };
                    let allow_deny_request = AllowOrDenyRequest::new(
                        response_sender,
                        AllowOrDeny::Deny,
                        self.servo_errors.sender(),
                    );
                    webview.delegate().request_protocol_handler(
                        webview,
                        protocol_handler_registration,
                        allow_deny_request,
                    );
                }
            },
            EmbedderMsg::AllowUnload(webview_id, response_sender) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    let request = AllowOrDenyRequest::new(
                        response_sender,
                        AllowOrDeny::Allow,
                        self.servo_errors.sender(),
                    );
                    webview.delegate().request_unload(webview, request);
                }
            },
            EmbedderMsg::InputEventsHandled(webview_id, event_outcomes) => {
                let webview = self.get_webview_handle(webview_id);
                for InputEventOutcome {
                    id: input_event_id,
                    result,
                } in event_outcomes
                {
                    self.paint.borrow_mut().notify_input_event_handled(
                        webview_id,
                        input_event_id,
                        result,
                    );
                    if let Some(ref webview) = webview {
                        webview.delegate().notify_input_event_handled(
                            webview.clone(),
                            input_event_id,
                            result,
                        );
                    }
                }
            },
            EmbedderMsg::ClearClipboard(webview_id) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    webview.clipboard_delegate().clear(webview);
                }
            },
            EmbedderMsg::GetClipboardText(webview_id, result_sender) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    webview
                        .clipboard_delegate()
                        .get_text(webview, StringRequest::from(result_sender));
                }
            },
            EmbedderMsg::SetClipboardText(webview_id, string) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    webview.clipboard_delegate().set_text(webview, string);
                }
            },
            EmbedderMsg::SetCursor(webview_id, cursor) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    webview.set_cursor(cursor);
                }
            },
            EmbedderMsg::NewFavicon(webview_id, image) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    webview.set_favicon(image);
                }
            },
            EmbedderMsg::NotifyLoadStatusChanged(webview_id, load_status) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    webview.set_load_status(load_status);
                }
            },
            EmbedderMsg::NotifyFullscreenStateChanged(webview_id, fullscreen) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    webview
                        .delegate()
                        .notify_fullscreen_state_changed(webview, fullscreen);
                }
            },
            EmbedderMsg::GetSelectedBluetoothDevice(webview_id, items, response_sender) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    webview.delegate().show_bluetooth_device_dialog(
                        webview,
                        BluetoothDeviceSelectionRequest::new(items, response_sender),
                    );
                }
            },
            EmbedderMsg::PromptPermission(webview_id, requested_feature, response_sender) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    let permission_request = PermissionRequest {
                        requested_feature,
                        allow_deny_request: AllowOrDenyRequest::new(
                            response_sender,
                            AllowOrDeny::Deny,
                            self.servo_errors.sender(),
                        ),
                    };
                    webview
                        .delegate()
                        .request_permission(webview, permission_request);
                }
            },
            EmbedderMsg::RequestWakeLockPermission(webview_id, callback, type_) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    let permission_request = PermissionRequest {
                        requested_feature: PermissionFeature::ScreenWakeLock(type_),
                        allow_deny_request: AllowOrDenyRequest::new_from_callback(
                            callback,
                            AllowOrDeny::Deny,
                            self.servo_errors.sender(),
                        ),
                    };
                    webview
                        .delegate()
                        .request_permission(webview, permission_request);
                }
            },
            EmbedderMsg::OnDevtoolsStarted(port, token) => match port {
                Ok(port) => self
                    .delegate
                    .borrow()
                    .notify_devtools_server_started(port, token),
                Err(()) => self
                    .delegate
                    .borrow()
                    .notify_error(ServoError::DevtoolsFailedToStart),
            },
            EmbedderMsg::RequestDevtoolsConnection(response_sender) => {
                self.delegate
                    .borrow()
                    .request_devtools_connection(AllowOrDenyRequest::new(
                        response_sender,
                        AllowOrDeny::Deny,
                        self.servo_errors.sender(),
                    ));
            },
            #[cfg(feature = "gamepad")]
            EmbedderMsg::PlayGamepadHapticEffect(
                webview_id,
                gamepad_index,
                gamepad_haptic_effect_type,
                callback,
            ) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    let request = GamepadHapticEffectRequest::new(
                        gamepad_index,
                        GamepadHapticEffectRequestType::Play(gamepad_haptic_effect_type),
                        Box::new(move |success| {
                            callback
                                .send(success)
                                .expect("Could not send message via callback")
                        }),
                    );
                    webview
                        .gamepad_delegate()
                        .handle_haptic_effect_request(request);
                }
            },
            #[cfg(feature = "gamepad")]
            EmbedderMsg::StopGamepadHapticEffect(webview_id, gamepad_index, callback) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    let request = GamepadHapticEffectRequest::new(
                        gamepad_index,
                        GamepadHapticEffectRequestType::Stop,
                        Box::new(move |success| {
                            callback
                                .send(success)
                                .expect("Could not send message via callback")
                        }),
                    );
                    webview
                        .gamepad_delegate()
                        .handle_haptic_effect_request(request);
                }
            },
            EmbedderMsg::ShowNotification(webview_id, notification) => {
                match webview_id.and_then(|webview_id| self.get_webview_handle(webview_id)) {
                    Some(webview) => webview.delegate().show_notification(webview, notification),
                    None => self.delegate.borrow().show_notification(notification),
                }
            },
            EmbedderMsg::ShowConsoleApiMessage(webview_id, level, message) => {
                match webview_id.and_then(|webview_id| self.get_webview_handle(webview_id)) {
                    Some(webview) => webview
                        .delegate()
                        .show_console_message(webview, level, message),
                    None => self.delegate.borrow().show_console_message(level, message),
                }
            },
            EmbedderMsg::ShowEmbedderControl(control_id, position, embedder_control_request) => {
                if let Some(webview) = self.get_webview_handle(control_id.webview_id) {
                    webview.show_embedder_control(control_id, position, embedder_control_request);
                }
            },
            EmbedderMsg::HideEmbedderControl(control_id) => {
                if let Some(webview) = self.get_webview_handle(control_id.webview_id) {
                    webview
                        .delegate()
                        .hide_embedder_control(webview, control_id);
                }
            },
            EmbedderMsg::GetWindowRect(webview_id, response_sender) => {
                let window_rect = || {
                    let Some(webview) = self.get_webview_handle(webview_id) else {
                        return DeviceIndependentIntRect::default();
                    };
                    let hidpi_scale_factor = webview.hidpi_scale_factor();
                    let Some(screen_geometry) = webview.delegate().screen_geometry(webview) else {
                        return DeviceIndependentIntRect::default();
                    };

                    convert_rect_to_css_pixel(screen_geometry.window_rect, hidpi_scale_factor)
                };

                if let Err(error) = response_sender.send(window_rect()) {
                    warn!("Failed to respond to GetWindowRect: {error}");
                }
            },
            EmbedderMsg::GetScreenMetrics(webview_id, response_sender) => {
                let screen_metrics = || {
                    let Some(webview) = self.get_webview_handle(webview_id) else {
                        return ScreenMetrics::default();
                    };
                    let hidpi_scale_factor = webview.hidpi_scale_factor();
                    let Some(screen_geometry) = webview.delegate().screen_geometry(webview) else {
                        return ScreenMetrics::default();
                    };

                    ScreenMetrics {
                        screen_size: convert_size_to_css_pixel(
                            screen_geometry.size,
                            hidpi_scale_factor,
                        ),
                        available_size: convert_size_to_css_pixel(
                            screen_geometry.available_size,
                            hidpi_scale_factor,
                        ),
                    }
                };
                if let Err(error) = response_sender.send(screen_metrics()) {
                    warn!("Failed to respond to GetScreenMetrics: {error}");
                }
            },
            EmbedderMsg::AccessibilityTreeUpdate(webview_id, tree_update, epoch) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    webview.process_accessibility_tree_update(tree_update, epoch);
                }
            },
        }
    }

    fn handle_constellation_embedder_message(&self, message: ConstellationToEmbedderMsg) {
        match message {
            ConstellationToEmbedderMsg::ShutdownComplete => self.finish_shutting_down(),
            ConstellationToEmbedderMsg::AllowNavigationRequest(
                webview_id,
                pipeline_id,
                servo_url,
                action,
            ) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    let request = NavigationRequest {
                        url: servo_url.into_url(),
                        action,
                        pipeline_id,
                        constellation_proxy: self.constellation_proxy.clone(),
                        response_sent: false,
                    };
                    webview.delegate().request_navigation(webview, request);
                }
            },
            ConstellationToEmbedderMsg::AllowOpeningWebView(
                webview_id,
                url,
                action,
                response_sender,
            ) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    webview.request_create_new(url.into_url(), action, response_sender);
                }
            },
            ConstellationToEmbedderMsg::WebViewClosed(webview_id) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    webview.delegate().notify_closed(webview);
                }
            },
            ConstellationToEmbedderMsg::FinishJavaScriptEvaluation(evaluation_id, result) => {
                self.javascript_evaluator
                    .borrow_mut()
                    .finish_evaluation(evaluation_id, result);
            },
            ConstellationToEmbedderMsg::InputEventsHandled(webview_id, event_outcomes) => {
                let webview = self.get_webview_handle(webview_id);
                for InputEventOutcome {
                    id: input_event_id,
                    result,
                } in event_outcomes
                {
                    self.paint.borrow_mut().notify_input_event_handled(
                        webview_id,
                        input_event_id,
                        result,
                    );
                    if let Some(ref webview) = webview {
                        webview.delegate().notify_input_event_handled(
                            webview.clone(),
                            input_event_id,
                            result,
                        );
                    }
                }
            },
            ConstellationToEmbedderMsg::HistoryTraversalComplete(webview_id, traversal_id) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    webview
                        .delegate()
                        .notify_traversal_complete(webview.clone(), traversal_id);
                }
            },
            ConstellationToEmbedderMsg::HistoryChanged(
                webview_id,
                new_back_forward_list,
                current_list_index,
            ) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    webview.set_history(new_back_forward_list, current_list_index);
                }
            },
            ConstellationToEmbedderMsg::Panic(webview_id, reason, backtrace) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    webview
                        .delegate()
                        .notify_crashed(webview, reason, backtrace);
                }
            },
            ConstellationToEmbedderMsg::RandomPipelineClosure(webview_id) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    webview.delegate().notify_random_pipeline_closure(webview);
                }
            },
            ConstellationToEmbedderMsg::MediaSessionEvent(webview_id, media_session_event) => {
                if let Some(webview) = self.get_webview_handle(webview_id) {
                    webview
                        .delegate()
                        .notify_media_session_event(webview, media_session_event);
                }
            },
        }
    }
}

impl Drop for ServoInner {
    fn drop(&mut self) {
        self.constellation_proxy
            .send(EmbedderToConstellationMessage::Exit);
        self.shutdown_state.set(ShutdownState::ShuttingDown);
        while self.spin_event_loop() {
            std::thread::sleep(Duration::from_micros(500));
        }
    }
}

/// An in-process handle to a `Servo` instance. Cloning the handle does not create a new instance
/// of `Servo`.
///
/// A `Servo` instance does everything necessary to render the web, primarily orchestrating the
/// interaction between JavaScript, CSS layout, rendering, and the client window.
///
/// Clients create an event loop to pump messages between the embedding
/// application and various browser components.
#[derive(Clone)]
pub struct Servo(Rc<ServoInner>);

impl Servo {
    #[servo_tracing::instrument(name = "Servo::new", skip(builder))]
    fn new(builder: ServoBuilder) -> Self {
        // Global configuration options, parsed from the command line.
        let opts = builder.opts.map(|opts| *opts).unwrap_or_default();
        opts::initialize_options(opts);
        let opts = opts::get();

        // Set the preferences globally.
        // TODO: It would be better to make these private to a particular Servo instance.
        let preferences = builder.preferences.map(|opts| *opts);
        servo_config::prefs::set(preferences.unwrap_or_default());

        use std::sync::atomic::Ordering;

        style::context::DEFAULT_DISABLE_STYLE_SHARING_CACHE.store(
            !pref!(layout_style_sharing_cache_enabled),
            Ordering::Relaxed,
        );
        style::context::DEFAULT_DUMP_STYLE_STATISTICS.store(
            opts.debug
                .is_enabled(DiagnosticsLoggingOption::StyleStatistics),
            Ordering::Relaxed,
        );

        if !opts.multiprocess {
            media_platform::init();
        }

        // Reserving a namespace to create WebViewId.
        PipelineNamespace::install(EMBEDDER_PIPELINE_NAMESPACE_ID);

        // Get both endpoints of a special channel for communication between
        // the client window and `Paint`. This channel is unique because
        // messages to client may need to pump a platform-specific event loop
        // to deliver the message.
        let event_loop_waker = builder.event_loop_waker;
        let (paint_proxy, paint_receiver) = create_paint_channel(event_loop_waker.clone());
        let (constellation_proxy, embedder_to_constellation_receiver) = ConstellationProxy::new();
        let (embedder_proxy, embedder_receiver) = create_embedder_channel(event_loop_waker.clone());
        let (net_embedder_proxy, net_embedder_receiver) =
            create_generic_embedder_channel::<NetToEmbedderMsg>(event_loop_waker.clone());
        let (constellation_embedder_proxy, constellation_embedder_receiver) =
            create_generic_embedder_channel::<ConstellationToEmbedderMsg>(event_loop_waker.clone());
        let time_profiler_chan = profile_time::Profiler::create(
            &opts.time_profiling,
            opts.time_profiler_trace_path.clone(),
        );
        let mem_profiler_chan = profile_mem::Profiler::create();

        let devtools_sender = if pref!(devtools_server_enabled) {
            Some(devtools::start_server(
                embedder_proxy.clone(),
                mem_profiler_chan.clone(),
            ))
        } else {
            None
        };

        // Important that this call is done in a single-threaded fashion, we
        // can't defer it after `create_constellation` has started.
        let js_engine_setup = if !opts.multiprocess {
            Some(script::init())
        } else {
            None
        };

        // Create the constellation, which maintains the engine pipelines, including script and
        // layout, as well as the navigation context.
        let mut protocols = ProtocolRegistry::with_internal_protocols();
        protocols.merge(builder.protocol_registry);

        // The `Paint` coordinates with the client window to create the final
        // rendered page and display it somewhere.
        let shutdown_state = Rc::new(Cell::new(ShutdownState::NotShuttingDown));
        let paint = Paint::new(InitialPaintState {
            paint_proxy: paint_proxy.clone(),
            receiver: paint_receiver,
            embedder_to_constellation_sender: constellation_proxy.sender(),
            time_profiler_chan: time_profiler_chan.clone(),
            mem_profiler_chan: mem_profiler_chan.clone(),
            shutdown_state: shutdown_state.clone(),
            event_loop_waker: event_loop_waker.clone(),
        });

        let protocols = Arc::new(protocols);
        let async_runtime = net::async_runtime::init_async_runtime();
        let partition_factory = PartitionFactory {
            devtools_sender: devtools_sender.clone(),
            time_profiler: time_profiler_chan.clone(),
            mem_profiler: mem_profiler_chan.clone(),
            embedder_proxy: net_embedder_proxy,
            certificates: opts
                .certificate_path
                .clone()
                .and_then(|path| {
                    load_root_cert_store_from_file(path)
                        .ok()
                        .map(CACertificates::Override)
                })
                .unwrap_or_default(),
            ignore_certificate_errors: opts.ignore_certificate_errors,
            protocols: protocols.clone(),
            engines: builder.storage_engines,
            cookie_callbacks: Rc::new(CookieCallbacks::default()),
        };
        let default_partition = partition_factory
            .create(
                StoragePartitionId::DEFAULT,
                if opts.temporary_storage {
                    None
                } else {
                    opts.config_dir.clone()
                },
            )
            .expect("Failed to initialize default storage partition");
        let public_resource_threads = default_partition.0.resources.clone();
        let public_storage_threads = default_partition.0.storage.clone();

        create_constellation(
            embedder_to_constellation_receiver,
            &paint.borrow(),
            embedder_proxy,
            constellation_embedder_proxy,
            paint_proxy,
            time_profiler_chan,
            mem_profiler_chan,
            devtools_sender,
            protocols,
            public_resource_threads.clone(),
            async_runtime,
            public_storage_threads.clone(),
        );

        net::connector::prewarm_tls();

        #[cfg(feature = "multiprocess")]
        if opts::get().multiprocess {
            prefs::add_observer(Box::new(constellation_proxy.clone()));
        }

        Servo(Rc::new(ServoInner {
            partitions: RefCell::new(
                [(default_partition.id(), default_partition.clone())]
                    .into_iter()
                    .collect(),
            ),
            default_partition,
            delegate: RefCell::new(Rc::new(DefaultServoDelegate)),
            paint,
            network_manager: Rc::new(RefCell::new(NetworkManager::new(
                public_resource_threads.clone(),
            ))),
            site_data_manager: SiteDataManager::new(
                public_resource_threads,
                public_storage_threads,
                Rc::clone(&partition_factory.cookie_callbacks),
            ),
            partition_factory,
            javascript_evaluator: Rc::new(RefCell::new(JavaScriptEvaluator::new(
                constellation_proxy.clone(),
            ))),
            constellation_proxy,
            embedder_receiver,
            net_embedder_receiver,
            constellation_embedder_receiver,
            shutdown_state,
            webviews: Default::default(),
            servo_errors: ServoErrorChannel::default(),
            _js_engine_setup: js_engine_setup,
            pending_handled_input_events: Default::default(),
            event_loop_waker,
        }))
    }

    pub fn delegate(&self) -> Rc<dyn ServoDelegate> {
        self.0.delegate.borrow().clone()
    }

    pub fn set_delegate(&self, delegate: Rc<dyn ServoDelegate>) {
        *self.0.delegate.borrow_mut() = delegate;
    }

    /// **EXPERIMENTAL:** Intialize GL accelerated media playback. This currently only works on a limited number
    /// of platforms. This should be run *before* creating [`Servo`] and its first [`WebView`].
    pub fn initialize_gl_accelerated_media(display: NativeDisplay, api: GlApi, context: GlContext) {
        WindowGLContext::initialize(display, api, context)
    }

    /// Spin the Servo event loop, which:
    ///
    ///   - Performs updates in `Paint`, such as queued pinch zoom events
    ///   - Runs delegate methods on all `WebView`s and `Servo` itself
    ///   - Maybe update the rendered `Paint` output, but *without* swapping buffers.
    pub fn spin_event_loop(&self) {
        self.0.spin_event_loop();
    }

    pub fn setup_logging(&self) {
        let constellation_chan = self.0.constellation_proxy.sender();
        let env = env_logger::Env::default();
        let env_logger = EnvLoggerBuilder::from_env(env).build();
        let con_logger = FromEmbedderLogger::new(constellation_chan);

        let filter = max(env_logger.filter(), con_logger.filter());
        let logger = BothLogger(env_logger, con_logger);

        log::set_boxed_logger(Box::new(logger)).expect("Failed to set logger.");
        log::set_max_level(filter);
    }

    pub fn create_memory_report(&self, snd: GenericCallback<MemoryReportResult>) {
        self.0
            .constellation_proxy
            .send(EmbedderToConstellationMessage::CreateMemoryReport(snd));
    }

    pub fn execute_webdriver_command(&self, command: WebDriverCommandMsg) {
        self.0
            .constellation_proxy
            .send(EmbedderToConstellationMessage::WebDriverCommand(command));
    }

    pub fn set_preference(&self, name: &str, value: PrefValue) {
        let mut preferences = prefs::get().clone();
        preferences.set_value(name, value);
        prefs::set(preferences);
    }

    pub fn network_manager<'a>(&'a self) -> Ref<'a, NetworkManager> {
        self.0.network_manager.borrow()
    }

    /// Manage the default partition (the legacy config_dir owner).
    pub fn site_data_manager(&self) -> &SiteDataManager {
        &self.0.site_data_manager
    }

    /// The compatibility partition selected by `config_dir` and `temporary_storage`.
    pub fn default_storage_partition(&self) -> StoragePartition {
        self.0.default_partition.clone()
    }

    /// Look up a registered owner without creating another owner for its root.
    pub fn storage_partition(&self, id: StoragePartitionId) -> Option<StoragePartition> {
        self.0.partitions.borrow().get(&id).cloned()
    }

    pub(crate) fn owns_storage_partition(&self, partition: &StoragePartition) -> bool {
        self.0
            .partitions
            .borrow()
            .get(&partition.id())
            .is_some_and(|owned| Rc::ptr_eq(&owned.0, &partition.0))
    }

    /// Create an independent owner. Reuse the returned handle for other WebViews;
    /// aliases of an already registered canonical root are rejected. This waits for
    /// initialization and constellation registration before returning the handle.
    pub fn create_storage_partition(
        &self,
        root: impl AsRef<Path>,
    ) -> Result<StoragePartition, StoragePartitionError> {
        std::fs::create_dir_all(root.as_ref())?;
        let root = std::fs::canonicalize(root)?;
        if self
            .0
            .partitions
            .borrow()
            .values()
            .any(|partition| partition.root() == Some(root.as_path()))
        {
            return Err(StoragePartitionError::DuplicateRoot(root));
        }
        self.register_storage_partition(Some(root))
    }

    /// Create an owner with no persistent profile data. Temporary SQLite files live
    /// until Servo shuts down; dropping the handle does not unload the partition.
    pub fn create_ephemeral_storage_partition(
        &self,
    ) -> Result<StoragePartition, StoragePartitionError> {
        self.register_storage_partition(None)
    }

    fn register_storage_partition(
        &self,
        root: Option<PathBuf>,
    ) -> Result<StoragePartition, StoragePartitionError> {
        if self.0.shutdown_state.get() != ShutdownState::NotShuttingDown {
            return Err(StoragePartitionError::ShuttingDown);
        }
        let partition = self
            .0
            .partition_factory
            .create(StoragePartitionId::new(), root)?;
        let (sender, receiver) = std::sync::mpsc::channel();
        self.constellation_proxy()
            .send(EmbedderToConstellationMessage::RegisterStoragePartition {
                resources: partition.0.resources.clone(),
                storage: partition.0.storage.clone(),
                response: sender,
            });
        let result = receiver
            .recv()
            .map_err(|error| error.to_string())
            .and_then(|result| result);
        if let Err(error) = result {
            let _ = partition.0.storage.exit();
            let (sender, receiver) = servo_base::generic_channel::oneshot().unwrap();
            if partition
                .0
                .resources
                .core_thread
                .send(net_traits::CoreResourceMsg::Exit(sender))
                .is_ok()
            {
                let _ = receiver.recv();
            }
            return Err(StoragePartitionError::Registration(error));
        }
        self.0
            .partitions
            .borrow_mut()
            .insert(partition.id(), partition.clone());
        Ok(partition)
    }

    /// When an [`ActionRequest`] is received from AccessKit, forward it to the appropriate document
    /// to fulfill the action in the request.
    ///
    /// For example, if an AccessKit adapter sends a request for an [`accesskit::Action::Click`]
    /// with a particular [`accesskit::TreeId`] and [`accesskit::NodeId`], determine which document
    /// contains the tree matching the [`accesskit::TreeId`] and forward the [`ActionRequest`]
    /// there, so that it can find the appropriate node to take the click action on.
    pub fn forward_accessibility_action(&self, action_request: ActionRequest) {
        self.0.constellation_proxy.send(
            EmbedderToConstellationMessage::ForwardAccessibilityAction(action_request),
        );
    }

    pub(crate) fn paint<'a>(&'a self) -> Ref<'a, Paint> {
        self.0.paint.borrow()
    }

    pub(crate) fn paint_mut<'a>(&'a self) -> RefMut<'a, Paint> {
        self.0.paint.borrow_mut()
    }

    pub(crate) fn event_loop_waker(&self) -> &dyn EventLoopWaker {
        &*self.0.event_loop_waker
    }

    pub(crate) fn webviews_mut<'a>(
        &'a self,
    ) -> RefMut<'a, FxHashMap<WebViewId, Weak<RefCell<WebViewInner>>>> {
        self.0.webviews.borrow_mut()
    }

    pub(crate) fn constellation_proxy(&self) -> &ConstellationProxy {
        &self.0.constellation_proxy
    }

    pub(crate) fn javascript_evaluator_mut<'a>(&'a self) -> RefMut<'a, JavaScriptEvaluator> {
        self.0.javascript_evaluator.borrow_mut()
    }

    pub(crate) fn add_pending_handled_input_event(&self, residue_event: PendingHandledInputEvent) {
        self.0
            .pending_handled_input_events
            .borrow_mut()
            .push(residue_event);
    }

    #[cfg(feature = "webxr")]
    /// Registers a [`WebXrRegistry`]
    pub fn register_webxr_registry(&self, registry: Box<dyn WebXrRegistry>) {
        self.0.paint.borrow().register_webxr_registry(registry);
    }
}

fn create_embedder_channel(
    event_loop_waker: Box<dyn EventLoopWaker>,
) -> (EmbedderProxy, Receiver<EmbedderMsg>) {
    let (sender, receiver) = unbounded();
    (
        EmbedderProxy {
            sender,
            event_loop_waker,
        },
        receiver,
    )
}

fn create_generic_embedder_channel<T>(
    event_loop_waker: Box<dyn EventLoopWaker>,
) -> (GenericEmbedderProxy<T>, Receiver<T>) {
    let (sender, receiver) = unbounded();
    (
        GenericEmbedderProxy {
            sender,
            event_loop_waker,
        },
        receiver,
    )
}

fn create_paint_channel(
    event_loop_waker: Box<dyn EventLoopWaker>,
) -> (PaintProxy, RoutedReceiver<PaintMessage>) {
    let (sender, receiver) = unbounded();
    let sender_clone = sender.clone();
    let event_loop_waker_clone = event_loop_waker.clone();
    // This callback is equivalent to `PaintProxy::send`
    let result_callback = move |msg: Result<PaintMessage, SendError>| {
        if let Err(err) = sender_clone.send(msg) {
            warn!("Failed to send response ({:?}).", err);
        }
        event_loop_waker_clone.wake();
    };

    let generic_callback =
        GenericCallback::new(result_callback).expect("Failed to create callback");
    let cross_process_paint_api = CrossProcessPaintApi::new(generic_callback);
    let paint_proxy = PaintProxy {
        sender,
        cross_process_paint_api,
        event_loop_waker,
    };

    (paint_proxy, receiver)
}

#[expect(clippy::too_many_arguments)]
fn create_constellation(
    embedder_to_constellation_receiver: Receiver<EmbedderToConstellationMessage>,
    paint: &Paint,
    embedder_proxy: EmbedderProxy,
    constellation_to_embedder_proxy: GenericEmbedderProxy<ConstellationToEmbedderMsg>,
    paint_proxy: PaintProxy,
    time_profiler_chan: time::ProfilerChan,
    mem_profiler_chan: mem::ProfilerChan,
    devtools_sender: Option<Sender<devtools_traits::DevtoolsControlMsg>>,
    protocols: Arc<ProtocolRegistry>,
    public_resource_threads: ResourceThreads,
    async_runtime: Box<dyn net_traits::AsyncRuntime>,
    public_storage_threads: StorageThreads,
) {
    // Global configuration options, parsed from the command line.
    let opts = opts::get();

    #[cfg(feature = "bluetooth")]
    let bluetooth_thread: GenericSender<BluetoothRequest> =
        BluetoothThreadFactory::new(embedder_proxy.clone());

    let privileged_urls = protocols.privileged_urls();

    let system_font_service = Arc::new(
        SystemFontService::spawn(
            paint_proxy.cross_process_paint_api.clone(),
            mem_profiler_chan.clone(),
        )
        .to_proxy(),
    );

    let initial_state = InitialConstellationState {
        paint_proxy,
        embedder_proxy,
        constellation_to_embedder_proxy,
        devtools_sender,
        #[cfg(feature = "bluetooth")]
        bluetooth_thread,
        system_font_service,
        public_resource_threads,
        public_storage_threads,
        time_profiler_chan,
        mem_profiler_chan,
        #[cfg(feature = "webxr")]
        webxr_registry: Some(paint.webxr_main_thread_registry()),
        #[cfg(not(feature = "webxr"))]
        webxr_registry: None,
        #[cfg(feature = "webgl")]
        webgl_threads: Some(paint.webgl_threads()),
        webrender_external_image_id_manager: paint.webrender_external_image_id_manager(),
        #[cfg(feature = "webgpu")]
        wgpu_image_map: paint.webgpu_image_map(),
        async_runtime,
        privileged_urls,
        wake_lock_provider: Box::new(DefaultWakeLockDelegate),
    };

    let layout_factory = Arc::new(LayoutFactoryImpl());

    Constellation::<script::ScriptThread, script::ServiceWorkerManager>::start(
        embedder_to_constellation_receiver,
        initial_state,
        layout_factory,
        opts.random_pipeline_closure_probability,
        opts.random_pipeline_closure_seed,
        opts.hard_fail,
    );
}

// A logger that logs to two downstream loggers.
// This should probably be in the log crate.
struct BothLogger<Log1, Log2>(Log1, Log2);

impl<Log1, Log2> Log for BothLogger<Log1, Log2>
where
    Log1: Log,
    Log2: Log,
{
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.0.enabled(metadata) || self.1.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        self.0.log(record);
        self.1.log(record);
    }

    fn flush(&self) {
        self.0.flush();
        self.1.flush();
    }
}

#[cfg(feature = "multiprocess")]
fn set_logger(script_to_constellation_sender: ScriptToConstellationSender) {
    let con_logger = FromScriptLogger::new(script_to_constellation_sender);
    let env = env_logger::Env::default();
    let env_logger = EnvLoggerBuilder::from_env(env).build();

    let filter = max(env_logger.filter(), con_logger.filter());
    let logger = BothLogger(env_logger, con_logger);

    log::set_boxed_logger(Box::new(logger)).expect("Failed to set logger.");
    log::set_max_level(filter);
}

fn content_process_diagnostic(enabled: bool, stage: &str) {
    if enabled {
        eprintln!("servo-content-diagnostic: {stage}");
    }
}

/// Content process entry point.
#[cfg(feature = "multiprocess")]
pub fn run_content_process(token: String) {
    let diagnostics = std::env::var_os("SERVO_CONTENT_PROCESS_DIAGNOSTICS").is_some();
    content_process_diagnostic(diagnostics, "entry");
    let (unprivileged_content_sender, unprivileged_content_receiver) =
        ipc::channel::<UnprivilegedContent>().unwrap();
    let connection_bootstrap: IpcSender<IpcSender<UnprivilegedContent>> =
        IpcSender::connect(token).unwrap();
    connection_bootstrap
        .send(unprivileged_content_sender)
        .unwrap();

    let unprivileged_content = unprivileged_content_receiver.recv().unwrap();
    content_process_diagnostic(diagnostics, "payload-received");
    opts::initialize_options(unprivileged_content.opts());
    prefs::set(unprivileged_content.prefs().clone());

    // Enter the sandbox if necessary.
    if opts::get().sandbox {
        content_process_diagnostic(diagnostics, "sandbox-activating");
        create_sandbox();
        content_process_diagnostic(diagnostics, "sandbox-active");
    }

    content_process_diagnostic(diagnostics, "js-initializing");
    let _js_engine_setup = script::init();
    content_process_diagnostic(diagnostics, "js-ready");

    match unprivileged_content {
        UnprivilegedContent::ScriptEventLoop(new_event_loop_info) => {
            if diagnostics {
                eprintln!(
                    "servo-content-diagnostic: script-event-loop reporter={}",
                    new_event_loop_info
                        .system_memory_reporter_name
                        .as_deref()
                        .unwrap_or("<missing>"),
                );
            }
            content_process_diagnostic(diagnostics, "media-initializing");
            media_platform::init();
            content_process_diagnostic(diagnostics, "media-ready");

            set_logger(
                new_event_loop_info
                    .initial_script_state
                    .script_to_constellation_sender
                    .clone(),
            );

            content_process_diagnostic(diagnostics, "memory-reporter-registering");
            register_system_memory_reporter_for_event_loop(&new_event_loop_info);
            content_process_diagnostic(diagnostics, "memory-reporter-registered");

            let (background_hang_monitor_register, background_hang_monitor_join_handle) =
                HangMonitorRegister::init(
                    new_event_loop_info.bhm_to_constellation_sender.clone(),
                    new_event_loop_info.constellation_to_bhm_receiver,
                    opts::get().background_hang_monitor,
                );

            let layout_factory = Arc::new(LayoutFactoryImpl());
            content_process_diagnostic(diagnostics, "script-thread-creating");
            let script_join_handle = script::ScriptThread::create(
                new_event_loop_info.initial_script_state,
                layout_factory,
                Arc::new(ImageCacheFactoryImpl::new(
                    new_event_loop_info.broken_image_icon_data,
                )),
                background_hang_monitor_register,
            );
            content_process_diagnostic(diagnostics, "script-thread-created");

            script_join_handle
                .join()
                .expect("Failed to join on the script thread.");
            background_hang_monitor_join_handle
                .join()
                .expect("Failed to join on the BHM background thread.");

            StyleThreadPool::shutdown();

            // Shut down the `FetchThread` if it had been started in the course of execution.
            content_process_diagnostic(diagnostics, "fetch-thread-exiting");
            FetchThread::exit();
            content_process_diagnostic(diagnostics, "fetch-thread-exited");
        },
        UnprivilegedContent::ServiceWorker(content) => {
            content.start::<ServiceWorkerManager>();
        },
    }
}

#[cfg(all(
    feature = "multiprocess",
    not(target_os = "windows"),
    not(target_os = "ios"),
    not(target_os = "android"),
    not(target_arch = "arm"),
    not(target_arch = "aarch64"),
    not(target_arch = "riscv32"),
    not(target_arch = "riscv64"),
    not(target_env = "ohos"),
))]
fn create_sandbox() {
    ChildSandbox::new(content_process_sandbox_profile())
        .activate()
        .expect("Failed to activate sandbox!");
}

#[cfg(any(
    target_os = "windows",
    target_os = "ios",
    target_os = "android",
    target_arch = "arm",
    target_arch = "aarch64",
    target_arch = "riscv32",
    target_arch = "riscv64",
    target_env = "ohos",
))]
fn create_sandbox() {
    panic!("Sandboxing is not supported on Windows, iOS, ARM, RISC-V targets and android.");
}

struct DefaultEventLoopWaker;

impl EventLoopWaker for DefaultEventLoopWaker {
    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(DefaultEventLoopWaker)
    }

    fn wake(&self) {}
}

/// Builder for [`Servo`].
pub struct ServoBuilder {
    opts: Option<Box<Opts>>,
    preferences: Option<Box<Preferences>>,
    event_loop_waker: Box<dyn EventLoopWaker>,
    protocol_registry: ProtocolRegistry,
    storage_engines: StorageEngines,
}

impl Default for ServoBuilder {
    fn default() -> Self {
        Self {
            opts: Default::default(),
            preferences: Default::default(),
            event_loop_waker: Box::new(DefaultEventLoopWaker),
            protocol_registry: Default::default(),
            storage_engines: Default::default(),
        }
    }
}

impl ServoBuilder {
    pub fn build(self) -> Servo {
        Servo::new(self)
    }

    pub fn opts(mut self, opts: Opts) -> Self {
        self.opts = Some(Box::new(opts));
        self
    }

    pub fn preferences(mut self, preferences: Preferences) -> Self {
        self.preferences = Some(Box::new(preferences));
        self
    }

    pub fn event_loop_waker(mut self, event_loop_waker: Box<dyn EventLoopWaker>) -> Self {
        self.event_loop_waker = event_loop_waker;
        self
    }

    pub fn protocol_registry(mut self, protocol_registry: ProtocolRegistry) -> Self {
        self.protocol_registry = protocol_registry;
        self
    }

    pub fn storage_engines(mut self, storage_engines: StorageEngines) -> Self {
        self.storage_engines = storage_engines;
        self
    }
}

#[cfg(feature = "multiprocess")]
fn register_system_memory_reporter_for_event_loop(
    new_event_loop_info: &NewScriptEventLoopProcessInfo,
) {
    // The parent assigns this name after spawning the process. A sandboxed Linux child is PID 1
    // in its own namespace, while the parent must unregister it by the host-visible PID.
    let reporter_name = new_event_loop_info
        .system_memory_reporter_name
        .as_ref()
        .expect("content process memory reporter name was not assigned by its parent");

    // Register the system memory reporter, which runs on its own thread. The parent unregisters
    // it when this content process exits.
    let callback = GenericCallback::new(|message| {
        if let Ok(request) = message {
            profile::system_reporter::collect_reports(request);
        }
    })
    .expect("Could not create memory reporter callback");
    new_event_loop_info
        .initial_script_state
        .memory_profiler_sender
        .send(ProfilerMsg::RegisterReporter(
            reporter_name.clone(),
            Reporter(callback),
        ));
}
