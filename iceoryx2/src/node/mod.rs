// Copyright (c) 2024 Contributors to the Eclipse Foundation
//
// See the NOTICE file(s) distributed with this work for additional
// information regarding copyright ownership.
//
// This program and the accompanying materials are made available under the
// terms of the Apache Software License 2.0 which is available at
// https://www.apache.org/licenses/LICENSE-2.0, or the MIT license
// which is available at https://opensource.org/licenses/MIT.
//
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The [`Node`](crate::node::Node) is the central entry point of iceoryx2. It is the owner of all communication
//! entities and provides additional memory to them to perform reference counting amongst other
//! things.
//!
//! It allows also the system to monitor the state of processes and cleanup stale resources of
//! dead processes.
//!
//! # Create a [`Node`](crate::node::Node)
//!
//! ```
//! use iceoryx2::prelude::*;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let node = NodeBuilder::new()
//!                 .name("my_little_node".try_into()?)
//!                 .create::<zero_copy::Service>()?;
//!
//! println!("created node {:?}", node);
//! # Ok(())
//! # }
//! ```
//!
//! # List all existing [`Node`](crate::node::Node)s
//!
//! ```
//! use iceoryx2::prelude::*;
//!
//! Node::<zero_copy::Service>::list(Config::get_global_config(), |node_state| {
//!     println!("found node {:?}", node_state);
//!     CallbackProgression::Continue
//! });
//! ```
//!
//! # Cleanup stale resources of all dead [`Node`](crate::node::Node)s
//!
//! ```
//! use iceoryx2::prelude::*;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! Node::<zero_copy::Service>::list(Config::get_global_config(), |node_state| {
//!     if let NodeState::<zero_copy::Service>::Dead(view) = node_state {
//!         println!("cleanup resources of dead node {:?}", view);
//!         if let Err(e) = view.remove_stale_resources() {
//!             println!("failed to cleanup resources due to {:?}", e);
//!         }
//!     }
//!     CallbackProgression::Continue
//! })?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Simple Event Loop
//!
//! ```no_run
//! use core::time::Duration;
//! use iceoryx2::prelude::*;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! const CYCLE_TIME: Duration = Duration::from_secs(1);
//! let node = NodeBuilder::new()
//!                 .name("my_little_node".try_into()?)
//!                 .create::<zero_copy::Service>()?;
//!
//! while let NodeEvent::Tick = node.wait(CYCLE_TIME) {
//!     // your algorithm in here
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ## Advanced Event Loop
//!
//! ```no_run
//! use core::time::Duration;
//! use iceoryx2::prelude::*;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! const CYCLE_TIME: Duration = Duration::from_secs(1);
//! let node = NodeBuilder::new()
//!                 .name("my_little_node".try_into()?)
//!                 .create::<zero_copy::Service>()?;
//!
//! loop {
//!     match node.wait(CYCLE_TIME) {
//!         NodeEvent::Tick => {
//!             println!("entered next cycle");
//!         }
//!         NodeEvent::TerminationRequest => {
//!             println!("User pressed CTRL+c, terminating");
//!             break;
//!         }
//!         NodeEvent::InterruptSignal => {
//!             println!("Someone send an interrupt signal ...");
//!         }
//!     }
//! }
//! # Ok(())
//! # }
//! ```

/// The name for a node.
pub mod node_name;

#[doc(hidden)]
pub mod testing;

use crate::node::node_name::NodeName;
use crate::service;
use crate::service::builder::{Builder, OpenDynamicStorageFailure};
use crate::service::config_scheme::{node_details_path, node_monitoring_config};
use crate::service::service_name::ServiceName;
use crate::{config::Config, service::config_scheme::node_details_config};
use iceoryx2_bb_container::semantic_string::SemanticString;
use iceoryx2_bb_elementary::CallbackProgression;
use iceoryx2_bb_lock_free::mpmc::container::ContainerHandle;
use iceoryx2_bb_log::{fail, fatal_panic, warn};
use iceoryx2_bb_posix::clock::{nanosleep, NanosleepError};
use iceoryx2_bb_posix::process::Process;
use iceoryx2_bb_posix::signal::SignalHandler;
use iceoryx2_bb_posix::unique_system_id::UniqueSystemId;
use iceoryx2_bb_system_types::file_name::FileName;
use iceoryx2_cal::named_concept::{NamedConceptPathHintRemoveError, NamedConceptRemoveError};
use iceoryx2_cal::{
    monitoring::*, named_concept::NamedConceptListError, serialize::*, static_storage::*,
};
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A complete list of all events that can occur in the main event loop, [`Node::wait()`].
#[derive(Debug, Eq, Hash, PartialEq, Clone, Copy)]
pub enum NodeEvent {
    /// The timeout passed.
    Tick,
    /// SIGTERM signal was received
    TerminationRequest,
    /// SIGINT signal was received
    InterruptSignal,
}

/// The system-wide unique id of a [`Node`]
#[derive(Debug, Eq, Hash, PartialEq, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct NodeId(UniqueSystemId);

impl NodeId {
    pub(crate) fn as_file_name(&self) -> FileName {
        fatal_panic!(from self, when FileName::new(self.0.to_string().as_bytes()),
                        "This should never happen! The NodeId shall be always a valid FileName.")
    }
}

/// The failures that can occur when a [`Node`] is created with the [`NodeBuilder`].
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum NodeCreationFailure {
    /// The [`Node`] could not be created since the process does not have sufficient permissions.
    InsufficientPermissions,
    /// Errors that indicate either an implementation issue or a wrongly configured system.
    InternalError,
}

impl std::fmt::Display for NodeCreationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::write!(f, "NodeCreationFailure::{:?}", self)
    }
}

impl std::error::Error for NodeCreationFailure {}

/// The failures that can occur when a list of [`NodeState`]s is created with [`Node::list()`].
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum NodeListFailure {
    /// A list of all [`Node`]s could not be created since the process does not have sufficient permissions.
    InsufficientPermissions,
    /// The process received an interrupt signal while acquiring the list of all [`Node`]s.
    Interrupt,
    /// Errors that indicate either an implementation issue or a wrongly configured system.
    InternalError,
}

impl std::fmt::Display for NodeListFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::write!(f, "NodeListFailure::{:?}", self)
    }
}

impl std::error::Error for NodeListFailure {}

/// Failures of [`DeadNodeView::remove_stale_resources()`] that occur when the stale resources of
/// a dead [`Node`] are removed.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum NodeCleanupFailure {
    /// The process received an interrupt signal while cleaning up all stale resources of a dead [`Node`].
    Interrupt,
    /// Errors that indicate either an implementation issue or a wrongly configured system.
    InternalError,
    /// The stale resources of a dead [`Node`] could not be removed since the process does not have sufficient permissions.
    InsufficientPermissions,
}

impl std::fmt::Display for NodeCleanupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::write!(f, "NodeCleanupFailure::{:?}", self)
    }
}

impl std::error::Error for NodeCleanupFailure {}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum NodeReadStorageFailure {
    ReadError,
    Corrupted,
    InternalError,
}

/// Optional detailed informations that a [`Node`] can have. They can only be obtained when the
/// process has sufficient access permissions.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NodeDetails {
    name: NodeName,
    config: Config,
}

impl NodeDetails {
    /// Returns the [`NodeName`]. Multiple [`Node`]s are allowed to have the same [`NodeName`], it
    /// is not unique!
    pub fn name(&self) -> &NodeName {
        &self.name
    }

    /// Returns the [`Config`] the [`Node`] uses to create all entities.
    pub fn config(&self) -> &Config {
        &self.config
    }
}

/// The current state of the [`Node`]. If the [`Node`] is dead all of its resources can be removed
/// with [`DeadNodeView::remove_stale_resources()`].
#[derive(Debug, Clone)]
pub enum NodeState<Service: service::Service> {
    /// The [`Node`]s process is still alive.
    Alive(AliveNodeView<Service>),
    /// The [`Node`]s process died without cleaning up the [`Node`]s resources. Another process has
    /// now the responsibility to cleanup all the stale resources.
    Dead(DeadNodeView<Service>),
    /// The process does not have sufficient permissions to identify the [`Node`] as dead or alive.
    Inaccessible(NodeId),
    /// The [`Node`] is in an undefined state, meaning that certain elements are missing,
    /// misconfigured or inconsistent. This can only happen due to an implementation failure or
    /// when the corresponding [`Node`] resources were altered.
    Undefined(NodeId),
}

impl<Service: service::Service> NodeState<Service> {
    pub(crate) fn new(node_id: &NodeId, config: &Config) -> Result<Option<Self>, NodeListFailure> {
        let details = match Node::<Service>::get_node_details(config, node_id) {
            Ok(v) => v,
            Err(_) => None,
        };

        let node_view = AliveNodeView::<Service> {
            id: *node_id,
            details,
            _service: PhantomData,
        };

        match Node::<Service>::get_node_state(config, node_id) {
            Ok(State::DoesNotExist) => Ok(None),
            Ok(State::Alive) => Ok(Some(NodeState::Alive(node_view))),
            Ok(State::Dead) => Ok(Some(NodeState::Dead(DeadNodeView(node_view)))),
            Err(NodeListFailure::InsufficientPermissions) => {
                Ok(Some(NodeState::Inaccessible(*node_id)))
            }
            Err(NodeListFailure::InternalError) => Ok(Some(NodeState::Undefined(*node_id))),
            Err(e) => Err(e),
        }
    }
}

/// Contains all available details of a [`Node`].
pub trait NodeView {
    /// Returns the [`NodeId`] of the [`Node`].
    fn id(&self) -> &NodeId;
    /// Returns the [`NodeDetails`].
    fn details(&self) -> &Option<NodeDetails>;
}

/// All the informations of a [`Node`] that is alive.
#[derive(Debug, Clone)]
pub struct AliveNodeView<Service: service::Service> {
    id: NodeId,
    details: Option<NodeDetails>,
    _service: PhantomData<Service>,
}

impl<Service: service::Service> NodeView for AliveNodeView<Service> {
    fn id(&self) -> &NodeId {
        &self.id
    }

    fn details(&self) -> &Option<NodeDetails> {
        &self.details
    }
}

/// All the informations and management operations belonging to a dead [`Node`].
#[derive(Debug, Clone)]
pub struct DeadNodeView<Service: service::Service>(AliveNodeView<Service>);

impl<Service: service::Service> NodeView for DeadNodeView<Service> {
    fn id(&self) -> &NodeId {
        self.0.id()
    }

    fn details(&self) -> &Option<NodeDetails> {
        self.0.details()
    }
}

impl<Service: service::Service> DeadNodeView<Service> {
    /// Removes all stale resources of a dead [`Node`].
    pub fn remove_stale_resources(self) -> Result<bool, NodeCleanupFailure> {
        let msg = "Unable to remove stale resources";
        let monitor_name = fatal_panic!(from self, when FileName::new(self.id().0.value().to_string().as_bytes()),
                                "This should never happen! {msg} since the NodeId is not a valid file name.");

        let config = if let Some(d) = self.details() {
            d.config()
        } else {
            Config::get_global_config()
        };

        let _cleaner = match <Service::Monitoring as Monitoring>::Builder::new(&monitor_name)
            .config(&node_monitoring_config::<Service>(config))
            .cleaner()
        {
            Ok(cleaner) => cleaner,
            Err(MonitoringCreateCleanerError::AlreadyOwnedByAnotherInstance)
            | Err(MonitoringCreateCleanerError::DoesNotExist) => return Ok(false),
            Err(MonitoringCreateCleanerError::Interrupt) => {
                fail!(from self, with NodeCleanupFailure::Interrupt,
                    "{} since an interrupt signal was received.", msg);
            }
            Err(MonitoringCreateCleanerError::InternalError) => {
                fail!(from self, with NodeCleanupFailure::InternalError,
                    "{} due to an internal error while acquiring monitoring cleaner.", msg);
            }
            Err(MonitoringCreateCleanerError::InstanceStillAlive) => {
                fatal_panic!(from self,
                        "This should never happen! {} since the Node is still alive.", msg);
            }
        };

        if let Some(details) = self.details() {
            remove_node::<Service>(*self.id(), details)
        } else {
            Ok(true)
        }
    }
}

fn acquire_all_node_detail_storages<Service: service::Service>(
    origin: &str,
    config: &<Service::StaticStorage as NamedConceptMgmt>::Configuration,
) -> Result<Vec<FileName>, NodeCleanupFailure> {
    let msg = "Unable to list all node detail storages";
    match <Service::StaticStorage as NamedConceptMgmt>::list_cfg(config) {
        Ok(v) => Ok(v),
        Err(NamedConceptListError::InsufficientPermissions) => {
            fail!(from origin, with NodeCleanupFailure::InsufficientPermissions,
                "{} due to insufficient permissions.", msg);
        }
        Err(NamedConceptListError::InternalError) => {
            fail!(from origin, with NodeCleanupFailure::InternalError,
                "{} due to an internal error.", msg);
        }
    }
}

fn remove_detail_storages<Service: service::Service>(
    origin: &str,
    storages: Vec<FileName>,
    config: &<Service::StaticStorage as NamedConceptMgmt>::Configuration,
) -> Result<(), NodeCleanupFailure> {
    let msg = "Unable to remove node detail storage";
    for entry in storages {
        match unsafe { <Service::StaticStorage as NamedConceptMgmt>::remove_cfg(&entry, config) } {
            Ok(_) => (),
            Err(NamedConceptRemoveError::InsufficientPermissions) => {
                fail!(from origin, with NodeCleanupFailure::InsufficientPermissions,
                    "{} {} due to insufficient permissions.", msg, entry);
            }
            Err(NamedConceptRemoveError::InternalError) => {
                fail!(from origin, with NodeCleanupFailure::InsufficientPermissions,
                    "{} {} due to an internal failure.", msg, entry);
            }
        }
    }

    Ok(())
}

fn remove_node_details_directory<Service: service::Service>(
    config: &Config,
    node_id: &NodeId,
) -> Result<(), NodeCleanupFailure> {
    let origin = format!("remove_node_details_directory({:?}, {:?})", config, node_id);
    let msg = "Unable to remove node details directory";
    let path = node_details_path(config, node_id);
    match <Service::StaticStorage as NamedConceptMgmt>::remove_path_hint(&path) {
        Ok(()) => Ok(()),
        Err(NamedConceptPathHintRemoveError::InsufficientPermissions) => {
            fail!(from origin, with NodeCleanupFailure::InsufficientPermissions,
                "{} due to insufficient permissions.", msg);
        }
        Err(NamedConceptPathHintRemoveError::InternalError) => {
            fail!(from origin, with NodeCleanupFailure::InternalError,
                "{} due to an internal error.", msg);
        }
    }
}

fn remove_node<Service: service::Service>(
    id: NodeId,
    details: &NodeDetails,
) -> Result<bool, NodeCleanupFailure> {
    let origin = format!(
        "remove_node<{}>({:?})",
        core::any::type_name::<Service>(),
        id
    );
    let details_config = node_details_config::<Service>(&details.config, &id);
    let detail_storages = acquire_all_node_detail_storages::<Service>(&origin, &details_config)?;
    remove_detail_storages::<Service>(&origin, detail_storages, &details_config)?;
    remove_node_details_directory::<Service>(details.config(), &id)?;

    Ok(true)
}

#[derive(Debug)]
pub(crate) struct RegisteredServices {
    data: Mutex<HashMap<String, (ContainerHandle, u64)>>,
}

impl RegisteredServices {
    pub(crate) fn add(&self, uuid: &str, handle: ContainerHandle) {
        if self
            .data
            .lock()
            .unwrap()
            .insert(uuid.to_string(), (handle, 1))
            .is_some()
        {
            fatal_panic!(from "RegisteredServices::add()",
                "This should never happen! The service with the uuid {} was already registered.", uuid);
        }
    }

    pub(crate) fn add_or<F: FnMut() -> Result<ContainerHandle, OpenDynamicStorageFailure>>(
        &self,
        uuid: &str,
        mut or_callback: F,
    ) -> Result<(), OpenDynamicStorageFailure> {
        let mut data = self.data.lock().unwrap();
        match data.get_mut(uuid) {
            Some(handle) => {
                handle.1 += 1;
            }
            None => {
                drop(data);
                let handle = or_callback()?;
                self.add(uuid, handle);
            }
        };
        Ok(())
    }

    pub(crate) fn remove<F: FnMut(ContainerHandle)>(&self, uuid: &str, mut cleanup_call: F) {
        let mut data = self.data.lock().unwrap();
        if let Some(entry) = data.get_mut(uuid) {
            entry.1 -= 1;
            if entry.1 == 0 {
                cleanup_call(entry.0);
                data.remove(uuid);
            }
        } else {
            fatal_panic!(from "RegisteredServices::remove()",
                "This should never happen! The service with the uuid {} was not registered.", uuid);
        }
    }
}

#[derive(Debug)]
pub(crate) struct SharedNode<Service: service::Service> {
    id: NodeId,
    details: NodeDetails,
    monitoring_token: UnsafeCell<Option<<Service::Monitoring as Monitoring>::Token>>,
    pub(crate) registered_services: RegisteredServices,
    _details_storage: Service::StaticStorage,
}

impl<Service: service::Service> SharedNode<Service> {
    pub(crate) fn config(&self) -> &Config {
        &self.details.config
    }

    pub(crate) fn id(&self) -> &NodeId {
        &self.id
    }
}

impl<Service: service::Service> Drop for SharedNode<Service> {
    fn drop(&mut self) {
        if self.monitoring_token.get_mut().is_some() {
            warn!(from self, when remove_node::<Service>(self.id, &self.details),
                "Unable to remove node resources.");
        }
    }
}

/// The [`Node`] is the entry point to the whole iceoryx2 infrastructure and owns all entities.
/// As soon as a process crashes other processes can detect dead [`Node`]s via [`Node::list()`]
/// and clean up the stale resources - the entities that
/// were created via the [`Node`].
///
/// Can be created via the [`NodeBuilder`].
#[derive(Debug)]
pub struct Node<Service: service::Service> {
    shared: Arc<SharedNode<Service>>,
}

unsafe impl<Service: service::Service> Send for Node<Service> {}

impl<Service: service::Service> Node<Service> {
    /// Returns the [`NodeName`].
    pub fn name(&self) -> &NodeName {
        &self.shared.details.name
    }

    /// Returns the [`Config`] that the [`Node`] will use to create any iceoryx2 entity.
    pub fn config(&self) -> &Config {
        &self.shared.details.config
    }

    /// Returns the [`NodeId`] of the [`Node`].
    pub fn id(&self) -> &NodeId {
        &self.shared.id
    }

    /// Instantiates a [`ServiceBuilder`](Builder) for a service with the provided name.
    pub fn service_builder(&self, name: ServiceName) -> Builder<Service> {
        Builder::new(name, self.shared.clone())
    }

    /// Calls the provided callback for all [`Node`]s in the system under a given [`Config`] and
    /// provides [`NodeState<Service>`] as input argument. With every iteration the callback has to
    /// return [`CallbackProgression::Continue`] to perform the next iteration or
    /// [`CallbackProgression::Stop`] to stop the iteration immediately.
    /// ```
    /// # use iceoryx2::prelude::*;
    /// Node::<zero_copy::Service>::list(Config::get_global_config(), |node_state| {
    ///     println!("found node {:?}", node_state);
    ///     CallbackProgression::Continue
    /// });
    /// ```
    pub fn list<F: FnMut(NodeState<Service>) -> CallbackProgression>(
        config: &Config,
        mut callback: F,
    ) -> Result<(), NodeListFailure> {
        let msg = "Unable to iterate over Node list";
        let origin = "Node::list()";
        let monitoring_config = node_monitoring_config::<Service>(config);

        match Self::list_all_nodes(&monitoring_config) {
            Ok(node_list) => {
                for node_name in node_list {
                    let node_id = core::str::from_utf8(node_name.as_bytes()).unwrap();
                    let node_id = NodeId(node_id.parse::<u128>().unwrap().into());

                    match NodeState::new(&node_id, config) {
                        Ok(Some(node_state)) => {
                            if callback(node_state) == CallbackProgression::Stop {
                                break;
                            }
                        }
                        Ok(None) => (),
                        Err(e) => {
                            fail!(from origin, with e,
                                "{msg} since the following error occurred ({:?}).", e);
                        }
                    }
                }
            }
            Err(e) => {
                fail!(from origin, with e,
                    "{msg} since the node list could not be acquired ({:?}).", e);
            }
        }

        Ok(())
    }

    /// # Safety
    ///
    ///  * only for internal testing purposes
    ///  * shall be called at most once
    ///
    pub(crate) unsafe fn staged_death(&mut self) -> <Service::Monitoring as Monitoring>::Token {
        (*self.shared.monitoring_token.get()).take().unwrap()
    }

    /// Waits until an event was received. It returns
    /// [`NodeEvent::Tick`] when the `cycle_time` has passed, otherwise event that occurred.
    pub fn wait(&self, cycle_time: Duration) -> NodeEvent {
        if SignalHandler::termination_requested() {
            return NodeEvent::TerminationRequest;
        }

        match nanosleep(cycle_time) {
            Ok(()) => {
                if SignalHandler::termination_requested() {
                    NodeEvent::TerminationRequest
                } else {
                    NodeEvent::Tick
                }
            }
            Err(NanosleepError::InterruptedBySignal(_)) => NodeEvent::InterruptSignal,
            Err(v) => {
                fatal_panic!(from self,
                    "Failed to wait with cycle time {:?} in main event look, caused by ({:?}).",
                    cycle_time, v);
            }
        }
    }

    fn list_all_nodes(
        config: &<Service::Monitoring as NamedConceptMgmt>::Configuration,
    ) -> Result<Vec<FileName>, NodeListFailure> {
        let result = <Service::Monitoring as NamedConceptMgmt>::list_cfg(config);

        if let Ok(result) = result {
            return Ok(result);
        }

        let msg = "Unable to list all nodes";
        let origin = format!("Node::list_all_nodes({:?})", config);
        match result.err().unwrap() {
            NamedConceptListError::InsufficientPermissions => {
                fail!(from origin, with NodeListFailure::InsufficientPermissions,
                        "{} due to insufficient permissions while listing all nodes.", msg);
            }
            NamedConceptListError::InternalError => {
                fail!(from origin, with NodeListFailure::InternalError,
                        "{} due to an internal failure while listing all nodes.", msg);
            }
        }
    }

    fn state_from_monitor(
        monitor: &<Service::Monitoring as Monitoring>::Monitor,
    ) -> Result<State, NodeListFailure> {
        let result = monitor.state();

        if let Ok(result) = result {
            return Ok(result);
        }

        let msg = "Unable to acquire node state from monitor";
        let origin = format!("Node::state_from_monitor({:?})", monitor);

        match result.err().unwrap() {
            MonitoringStateError::Interrupt => {
                fail!(from origin, with NodeListFailure::Interrupt,
                    "{} due to an interrupt signal while acquiring the nodes state.", msg);
            }
            MonitoringStateError::InternalError => {
                fail!(from origin, with NodeListFailure::InternalError,
                    "{} due to an internal error while acquiring the nodes state.", msg);
            }
        }
    }

    fn get_node_state(config: &Config, node_id: &NodeId) -> Result<State, NodeListFailure> {
        let my_pid = Process::from_self().id();
        let node_pid = node_id.0.pid();

        if my_pid == node_pid {
            return Ok(State::Alive);
        }

        let config = node_monitoring_config::<Service>(config);
        let result = <Service::Monitoring as Monitoring>::Builder::new(&node_id.as_file_name())
            .config(&config)
            .monitor();

        if let Ok(result) = result {
            return Self::state_from_monitor(&result);
        }

        let msg = "Unable to acquire node monitor";
        let origin = format!("Node::get_node_state({:?}, {:?})", config, node_id);
        match result.err().unwrap() {
            MonitoringCreateMonitorError::InsufficientPermissions => {
                fail!(from origin, with NodeListFailure::InsufficientPermissions,
                        "{} due to insufficient permissions while acquiring the node state.", msg);
            }
            MonitoringCreateMonitorError::Interrupt => {
                fail!(from origin, with NodeListFailure::Interrupt,
                        "{} since an interrupt was received while acquiring the node state.", msg);
            }
            MonitoringCreateMonitorError::InternalError => {
                fail!(from origin, with NodeListFailure::InternalError,
                        "{} since an internal failure occurred while acquiring the node state.", msg);
            }
        }
    }

    fn open_node_storage(
        config: &Config,
        node_id: &NodeId,
    ) -> Result<Option<Service::StaticStorage>, NodeReadStorageFailure> {
        let details_config = node_details_config::<Service>(config, node_id);
        let result = <Service::StaticStorage as StaticStorage>::Builder::new(
            &FileName::new(b"node").unwrap(),
        )
        .config(&details_config)
        .has_ownership(false)
        .open();

        if let Ok(result) = result {
            return Ok(Some(result));
        }

        let msg = "Unable to open node config storage";
        let origin = format!("open_node_storage({:?}, {:?})", config, node_id);

        match result.err().unwrap() {
            StaticStorageOpenError::DoesNotExist => Ok(None),
            StaticStorageOpenError::Read => {
                fail!(from origin, with NodeReadStorageFailure::ReadError,
                    "{} since the node config storage could not be read.", msg);
            }
            StaticStorageOpenError::IsLocked => {
                fail!(from origin, with NodeReadStorageFailure::Corrupted,
                    "{} since the node config storage seems to be uninitialized but the state should always be present.", msg);
            }
            StaticStorageOpenError::InternalError => {
                fail!(from origin, with NodeReadStorageFailure::InternalError,
                    "{} due to an internal failure while opening the node config storage.", msg);
            }
        }
    }

    fn get_node_details(
        config: &Config,
        node_id: &NodeId,
    ) -> Result<Option<NodeDetails>, NodeReadStorageFailure> {
        let node_storage = if let Some(n) = Self::open_node_storage(config, node_id)? {
            n
        } else {
            return Ok(None);
        };

        let mut read_content =
            String::from_utf8(vec![b' '; node_storage.len() as usize]).expect("");

        let origin = format!("get_node_details({:?}, {:?})", config, node_id);
        let msg = "Unable to read node details";

        if node_storage
            .read(unsafe { read_content.as_mut_vec() }.as_mut_slice())
            .is_err()
        {
            fail!(from origin, with NodeReadStorageFailure::ReadError,
                "{} since the content of the node config storage could not be read.", msg);
        }

        let node_details = fail!(from origin,
                    when Service::ConfigSerializer::deserialize::<NodeDetails>(unsafe { read_content.as_mut_vec()}),
                    with NodeReadStorageFailure::Corrupted,
                "{} since the contents of the node config storage is corrupted.", msg);

        Ok(Some(node_details))
    }
}

/// Creates a [`Node`].
///
/// ```
/// use iceoryx2::prelude::*;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let node = NodeBuilder::new()
///                 .name("my_little_node".try_into()?)
///                 .create::<zero_copy::Service>()?;
///
/// // do things with your cool new node
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Default)]
pub struct NodeBuilder {
    name: Option<NodeName>,
    config: Option<Config>,
}

impl NodeBuilder {
    /// Creates a new [`NodeBuilder`]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the [`NodeName`] of the to be created [`Node`].
    pub fn name(mut self, value: NodeName) -> Self {
        self.name = Some(value);
        self
    }

    /// Sets the config of the [`Node`] that will be used to create all entities owned by the
    /// [`Node`].
    pub fn config(mut self, value: &Config) -> Self {
        self.config = Some(value.clone());
        self
    }

    /// Creates a new [`Node`] for a specific [`service::Service`]. All entities owned by the
    /// [`Node`] will have the same [`service::Service`].
    pub fn create<Service: service::Service>(self) -> Result<Node<Service>, NodeCreationFailure> {
        let msg = "Unable to create node";
        let node_id = fail!(from self, when UniqueSystemId::new(),
                                with NodeCreationFailure::InternalError,
                                "{msg} since the unique node id could not be generated.");
        let monitor_name = fatal_panic!(from self, when FileName::new(node_id.value().to_string().as_bytes()),
                                "This should never happen! {msg} since the UniqueSystemId is not a valid file name.");
        let config = if let Some(ref config) = self.config {
            config.clone()
        } else {
            Config::get_global_config().clone()
        };

        let (details_storage, details) =
            self.create_node_details_storage::<Service>(&config, &NodeId(node_id))?;
        let monitoring_token = self.create_token::<Service>(&config, &monitor_name)?;

        Ok(Node {
            shared: Arc::new(SharedNode {
                id: NodeId(node_id),
                monitoring_token: UnsafeCell::new(Some(monitoring_token)),
                registered_services: RegisteredServices {
                    data: Mutex::new(HashMap::new()),
                },
                _details_storage: details_storage,
                details,
            }),
        })
    }

    fn create_token<Service: service::Service>(
        &self,
        config: &Config,
        monitor_name: &FileName,
    ) -> Result<<Service::Monitoring as Monitoring>::Token, NodeCreationFailure> {
        let msg = "Unable to create token for new node";
        let token_result = <Service::Monitoring as Monitoring>::Builder::new(monitor_name)
            .config(&node_monitoring_config::<Service>(config))
            .token();

        match token_result {
            Ok(token) => Ok(token),
            Err(MonitoringCreateTokenError::InsufficientPermissions) => {
                fail!(from self, with NodeCreationFailure::InsufficientPermissions,
                    "{msg} due to insufficient permissions to create a monitor token.");
            }
            Err(MonitoringCreateTokenError::AlreadyExists) => {
                fatal_panic!(from self,
                    "This should never happen! {msg} since a node with the same UniqueNodeId already exists.");
            }
            Err(MonitoringCreateTokenError::InternalError) => {
                fail!(from self, with NodeCreationFailure::InternalError,
                    "{msg} since the monitor token could not be created.");
            }
        }
    }

    fn create_node_details_storage<Service: service::Service>(
        &self,
        config: &Config,
        node_id: &NodeId,
    ) -> Result<(Service::StaticStorage, NodeDetails), NodeCreationFailure> {
        let msg = "Unable to create node details storage";
        let details = NodeDetails {
            name: if let Some(ref name) = self.name {
                name.clone()
            } else {
                NodeName::new("").expect("An empty NodeName is always valid.")
            },
            config: config.clone(),
        };

        let details_config = node_details_config::<Service>(&details.config, node_id);
        let serialized_details = match <Service::ConfigSerializer>::serialize(&details) {
            Ok(serialized_details) => serialized_details,
            Err(SerializeError::InternalError) => {
                fail!(from self, with NodeCreationFailure::InternalError,
                    "{msg} since the node details could not be serialized.");
            }
        };

        match <Service::StaticStorage as StaticStorage>::Builder::new(
            &FileName::new(b"node").unwrap(),
        )
        .config(&details_config)
        .has_ownership(false)
        .create(&serialized_details)
        {
            Ok(node_details) => Ok((node_details, details)),
            Err(StaticStorageCreateError::InsufficientPermissions) => {
                fail!(from self, with NodeCreationFailure::InsufficientPermissions,
                    "{msg} due to insufficient permissions to create the node details file.");
            }
            Err(StaticStorageCreateError::AlreadyExists) => {
                fatal_panic!(from self,
                    "This should never happen! {msg} since the node details file already exists.");
            }
            Err(e) => {
                fail!(from self, with NodeCreationFailure::InternalError,
                    "{msg} due to an unknown failure while creating the node details file {:?}.", e);
            }
        }
    }
}