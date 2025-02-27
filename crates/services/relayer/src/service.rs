//! This module handles bridge communications between the fuel node and the data availability layer.

use crate::{
    log::EthEventLog,
    ports::RelayerDb,
    service::state::EthLocal,
    Config,
};
use async_trait::async_trait;
use core::time::Duration;
use ethers_core::types::{
    Filter,
    Log,
    SyncingStatus,
    ValueOrArray,
    H160,
};
use ethers_providers::{
    Http,
    Middleware,
    Provider,
    ProviderError,
};
use fuel_core_services::{
    RunnableService,
    RunnableTask,
    ServiceRunner,
    StateWatcher,
};
use fuel_core_storage::{
    tables::Messages,
    StorageAsRef,
    StorageInspect,
};
use fuel_core_types::{
    blockchain::primitives::DaBlockHeight,
    entities::message::Message,
    fuel_types::Nonce,
};
use futures::StreamExt;
use std::{
    borrow::Cow,
    convert::TryInto,
    ops::Deref,
};
use synced::update_synced;
use tokio::sync::watch;

use self::{
    get_logs::*,
    run::RelayerData,
};

mod get_logs;
mod run;
mod state;
mod synced;
mod syncing;

#[cfg(test)]
mod test;

type Synced = watch::Receiver<Option<DaBlockHeight>>;
type NotifySynced = watch::Sender<Option<DaBlockHeight>>;

/// The alias of runnable relayer service.
pub type Service<D> = CustomizableService<Provider<Http>, D>;
type CustomizableService<P, D> = ServiceRunner<NotInitializedTask<P, D>>;

/// The shared state of the relayer task.
#[derive(Clone)]
pub struct SharedState<D> {
    /// Receives signals when the relayer reaches consistency with the DA layer.
    synced: Synced,
    database: D,
}

/// Not initialized version of the [`Task`].
pub struct NotInitializedTask<P, D> {
    /// Sends signals when the relayer reaches consistency with the DA layer.
    synced: NotifySynced,
    /// The node that communicates with Ethereum.
    eth_node: P,
    /// The fuel database.
    database: D,
    /// Configuration settings.
    config: Config,
}

/// The actual relayer background task that syncs with the DA layer.
pub struct Task<P, D> {
    /// Sends signals when the relayer reaches consistency with the DA layer.
    synced: NotifySynced,
    /// The node that communicates with Ethereum.
    eth_node: P,
    /// The fuel database.
    database: D,
    /// Configuration settings.
    config: Config,
    /// The watcher used to track the state of the service. If the service stops,
    /// the task will stop synchronization.
    shutdown: StateWatcher,
}

impl<P, D> NotInitializedTask<P, D> {
    /// Create a new relayer task.
    fn new(eth_node: P, database: D, config: Config) -> Self {
        let (synced, _) = watch::channel(None);
        Self {
            synced,
            eth_node,
            database,
            config,
        }
    }
}

impl<P, D> Task<P, D>
where
    D: RelayerDb + 'static,
{
    fn set_deploy_height(&mut self) {
        self.database
            .set_finalized_da_height_to_at_least(&self.config.da_deploy_height)
            .expect("Should be able to set the finalized da height");
    }
}

#[async_trait]
impl<P, D> RelayerData for Task<P, D>
where
    P: Middleware<Error = ProviderError> + 'static,
    D: RelayerDb + 'static,
{
    async fn wait_if_eth_syncing(&self) -> anyhow::Result<()> {
        let mut shutdown = self.shutdown.clone();
        tokio::select! {
            biased;
            _ = shutdown.while_started() => {
                Err(anyhow::anyhow!("The relayer got a stop signal"))
            },
            result = syncing::wait_if_eth_syncing(
                &self.eth_node,
                self.config.syncing_call_frequency,
                self.config.syncing_log_frequency,
            ) => {
                result
            }
        }
    }

    async fn download_logs(
        &mut self,
        eth_sync_gap: &state::EthSyncGap,
    ) -> anyhow::Result<()> {
        let logs = download_logs(
            eth_sync_gap,
            self.config.eth_v2_listening_contracts.clone(),
            &self.eth_node,
            self.config.log_page_size,
        );
        let logs = logs.take_until(self.shutdown.while_started());
        write_logs(&mut self.database, logs).await
    }

    fn update_synced(&self, state: &state::EthState) {
        update_synced(&self.synced, state)
    }
}

#[async_trait]
impl<P, D> RunnableService for NotInitializedTask<P, D>
where
    P: Middleware<Error = ProviderError> + 'static,
    D: RelayerDb + Clone + 'static,
{
    const NAME: &'static str = "Relayer";

    type SharedData = SharedState<D>;
    type Task = Task<P, D>;
    type TaskParams = ();

    fn shared_data(&self) -> Self::SharedData {
        let synced = self.synced.subscribe();

        SharedState {
            synced,
            database: self.database.clone(),
        }
    }

    async fn into_task(
        mut self,
        watcher: &StateWatcher,
        _: Self::TaskParams,
    ) -> anyhow::Result<Self::Task> {
        let shutdown = watcher.clone();
        let NotInitializedTask {
            synced,
            eth_node,
            database,
            config,
        } = self;
        let mut task = Task {
            synced,
            eth_node,
            database,
            config,
            shutdown,
        };
        task.set_deploy_height();

        Ok(task)
    }
}

#[async_trait]
impl<P, D> RunnableTask for Task<P, D>
where
    P: Middleware<Error = ProviderError> + 'static,
    D: RelayerDb + 'static,
{
    async fn run(&mut self, _: &mut StateWatcher) -> anyhow::Result<bool> {
        let now = tokio::time::Instant::now();
        let should_continue = true;

        let result = run::run(self).await;

        if self.shutdown.borrow_and_update().started()
            && (result.is_err() | self.synced.borrow().is_some())
        {
            // Sleep the loop so the da node is not spammed.
            tokio::time::sleep(
                self.config
                    .sync_minimum_duration
                    .saturating_sub(now.elapsed()),
            )
            .await;
        }

        result.map(|_| should_continue)
    }

    async fn shutdown(self) -> anyhow::Result<()> {
        // Nothing to shut down because we don't have any temporary state that should be dumped,
        // and we don't spawn any sub-tasks that we need to finish or await.
        Ok(())
    }
}

impl<D> SharedState<D> {
    /// Wait for the [`Task`] to be in sync with
    /// the data availability layer.
    ///
    /// Yields until the relayer reaches a point where it
    /// considered up to date. Note that there's no guarantee
    /// the relayer will ever catch up to the da layer and
    /// may fall behind immediately after this future completes.
    ///
    /// The only guarantee is that if this future completes then
    /// the relayer did reach consistency with the da layer for
    /// some period of time.
    pub async fn await_synced(&self) -> anyhow::Result<()> {
        let mut rx = self.synced.clone();
        if rx.borrow_and_update().deref().is_none() {
            rx.changed().await?;
        }
        Ok(())
    }

    /// Wait until at least the given height is synced.
    pub async fn await_at_least_synced(
        &self,
        height: &DaBlockHeight,
    ) -> anyhow::Result<()> {
        let mut rx = self.synced.clone();
        while rx.borrow_and_update().deref().map_or(true, |h| h < *height) {
            rx.changed().await?;
        }
        Ok(())
    }

    /// Get a message if it has been synced
    /// and is <= the given height.
    pub fn get_message(
        &self,
        id: &Nonce,
        da_height: &DaBlockHeight,
    ) -> anyhow::Result<Option<Message>>
    where
        D: StorageInspect<Messages, Error = fuel_core_storage::Error>,
    {
        Ok(self
            .database
            .storage::<Messages>()
            .get(id)?
            .map(Cow::into_owned)
            .filter(|message| message.da_height <= *da_height))
    }

    /// Get finalized da height that represents last block from da layer that got finalized.
    /// Panics if height is not set as of initialization of the relayer.
    pub fn get_finalized_da_height(&self) -> anyhow::Result<DaBlockHeight>
    where
        D: RelayerDb + 'static,
    {
        Ok(self.database.get_finalized_da_height()?)
    }
}

#[async_trait]
impl<P, D> state::EthRemote for Task<P, D>
where
    P: Middleware<Error = ProviderError>,
    D: RelayerDb + 'static,
{
    async fn finalized(&self) -> anyhow::Result<u64> {
        let mut shutdown = self.shutdown.clone();
        tokio::select! {
            biased;
            _ = shutdown.while_started() => {
                Err(anyhow::anyhow!("The relayer got a stop signal"))
            },
            block = self.eth_node.get_block(ethers_core::types::BlockNumber::Finalized) => {
                let block_number = block?
                    .and_then(|block| block.number)
                    .ok_or(anyhow::anyhow!("Block pending"))?
                    .as_u64();
                Ok(block_number)
            }
        }
    }
}

#[async_trait]
impl<P, D> EthLocal for Task<P, D>
where
    P: Middleware<Error = ProviderError>,
    D: RelayerDb + 'static,
{
    fn observed(&self) -> Option<u64> {
        self.database.get_finalized_da_height().map(|h| *h).ok()
    }
}

/// Creates an instance of runnable relayer service.
pub fn new_service<D>(database: D, config: Config) -> anyhow::Result<Service<D>>
where
    D: RelayerDb + Clone + 'static,
{
    let url = config.relayer.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "Tried to start Relayer without setting an eth_client in the config"
        )
    })?;
    // TODO: Does this handle https?
    let http = Http::new(url);
    let eth_node = Provider::new(http);
    Ok(new_service_internal(eth_node, database, config))
}

#[cfg(any(test, feature = "test-helpers"))]
/// Start a test relayer.
pub fn new_service_test<P, D>(
    eth_node: P,
    database: D,
    config: Config,
) -> CustomizableService<P, D>
where
    P: Middleware<Error = ProviderError> + 'static,
    D: RelayerDb + Clone + 'static,
{
    new_service_internal(eth_node, database, config)
}

fn new_service_internal<P, D>(
    eth_node: P,
    database: D,
    config: Config,
) -> CustomizableService<P, D>
where
    P: Middleware<Error = ProviderError> + 'static,
    D: RelayerDb + Clone + 'static,
{
    let task = NotInitializedTask::new(eth_node, database, config);

    CustomizableService::new(task)
}
