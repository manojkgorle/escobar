//! Implementation of [`snowman.block.ChainVM`](https://pkg.go.dev/github.com/ava-labs/avalanchego/snow/engine/snowman/block#ChainVM) interface for timestampvm.

use std::{
    collections::{HashMap, VecDeque},
    io::{self, Error, ErrorKind},
    sync::Arc,
    time::Duration,
};

use crate::{
    api::{
        chain_handlers::{ChainHandler, ChainService},
        static_handlers::{StaticHandler, StaticService},
    },
    block::{deploy_tx::DeployTx, Block},
    genesis::Genesis,
    state,
};
use avalanche_types::{
    choices, ids,
    subnet::{
        self,
        rpc::{
            context::Context,
            database::{manager::DatabaseManager, BoxedDatabase},
            health::Checkable,
            snow::{
                self,
                engine::common::{
                    appsender::AppSender,
                    engine::{AppHandler, CrossChainAppHandler, NetworkAppHandler},
                    http_handler::{HttpHandler, LockOptions},
                    vm::{CommonVm, Connector},
                },
                validators::client::ValidatorStateClient,
            },
            snowman::block::{BatchedChainVm, ChainVm, Getter, Parser},
        },
    },
};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use semver::Version;
use solana_sdk::account::AccountSharedData;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::SanitizedTransaction;
use tokio::sync::{mpsc::Sender, RwLock};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Limits how much data a user can propose.
pub const PROPOSE_LIMIT_BYTES: usize = 1024 * 1024; // @todo this should be configured.

/// Represents VM-specific states.
/// Defined in a separate struct, for interior mutability in [`Vm`](Vm).
/// To be protected with `Arc` and `RwLock`.
pub struct State {
    pub ctx: Option<Context<ValidatorStateClient>>,
    pub version: Version,
    pub genesis: Genesis,

    /// Represents persistent Vm state.
    pub state: Option<state::State>,
    /// Currently preferred block Id.
    pub preferred: ids::Id,
    /// Channel to send messages to the snowman consensus engine.
    pub to_engine: Option<Sender<snow::engine::common::message::Message>>,
    /// Set "true" to indicate that the Vm has finished bootstrapping
    /// for the chain.
    pub bootstrapped: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            ctx: None,
            version: Version::new(0, 0, 0),
            genesis: Genesis::default(),

            state: None,
            preferred: ids::Id::empty(),
            to_engine: None,
            bootstrapped: false,
        }
    }
}

/// Implements [`snowman.block.ChainVM`](https://pkg.go.dev/github.com/ava-labs/avalanchego/snow/engine/snowman/block#ChainVM) interface.
#[derive(Clone)]
pub struct Vm<A> {
    /// Maintains the Vm-specific states.
    pub state: Arc<RwLock<State>>,
    pub app_sender: Option<A>,

    /// A queue of data that have not been put into a block and proposed yet.
    /// Mempool is not persistent, so just keep in memory via Vm.
    pub mempool: Arc<RwLock<VecDeque<SanitizedTransaction>>>,
    pub deploy_mempool: Arc<RwLock<VecDeque<DeployTx>>>,
}

impl<A> Default for Vm<A>
where
    A: Send + Sync + Clone + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<A> Vm<A>
where
    A: Send + Sync + Clone + 'static,
{
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(State::default())),
            app_sender: None,
            mempool: Arc::new(RwLock::new(VecDeque::with_capacity(100))),
            deploy_mempool: Arc::new(RwLock::new(VecDeque::with_capacity(100))),
        }
    }

    pub async fn is_bootstrapped(&self) -> bool {
        let vm_state = self.state.read().await;
        vm_state.bootstrapped
    }

    /// Signals the consensus engine that a new block is ready to be created.
    pub async fn notify_block_ready(&self) {
        // @todo go with block builder method.
        let vm_state = self.state.read().await;
        if let Some(to_engine) = &vm_state.to_engine {
            to_engine
                .send(snow::engine::common::message::Message::PendingTxs)
                .await
                .unwrap_or_else(|e| log::warn!("dropping message to consensus engine: {}", e));

            log::info!("notified block ready!");
        } else {
            log::error!("consensus engine channel failed to initialized");
        }
    }

    /// Proposes arbitrary data to mempool and notifies that a block is ready for builds.
    /// Other VMs may optimize mempool with more complicated batching mechanisms.
    /// # Errors
    /// Can fail if the data size exceeds `PROPOSE_LIMIT_BYTES`.
    pub async fn propose_block_with_deploy_program(&self, d: &DeployTx) -> io::Result<()> {
        let mut mempool = self.deploy_mempool.write().await;
        mempool.push_back(d.clone());
        log::info!("deploy program added to mempool");

        self.notify_block_ready().await;
        Ok(())
    }

    pub async fn propose_block_with_multi_txs(
        &self,
        txs: Vec<SanitizedTransaction>,
    ) -> io::Result<()> {
        let mut mempool = self.mempool.write().await;
        for tx in txs {
            mempool.push_back(tx);
        }
        log::info!("multi txs added to mempool");

        self.notify_block_ready().await;
        Ok(())
    }
    /// Sets the state of the Vm.
    /// # Errors
    /// Will fail if the `snow::State` is syncing
    pub async fn set_state(&self, snow_state: snow::State) -> io::Result<()> {
        let mut vm_state = self.state.write().await;
        match snow_state {
            // called by chains manager when it is creating the chain.
            snow::State::Initializing => {
                log::info!("set_state: initializing");
                vm_state.bootstrapped = false;
                Ok(())
            }

            snow::State::StateSyncing => {
                log::info!("set_state: state syncing");
                Err(Error::new(ErrorKind::Other, "state sync is not supported"))
            }

            // called by the bootstrapper to signal bootstrapping has started.
            snow::State::Bootstrapping => {
                log::info!("set_state: bootstrapping");
                vm_state.bootstrapped = false;
                Ok(())
            }

            // called when consensus has started signalling bootstrap phase is complete.
            snow::State::NormalOp => {
                log::info!("set_state: normal op");
                vm_state.bootstrapped = true;
                Ok(())
            }
        }
    }

    /// Returns the last accepted block Id.
    /// # Errors
    /// Will fail if there's no state or if the db can't be accessed
    pub async fn last_accepted(&self) -> io::Result<ids::Id> {
        let vm_state = self.state.read().await;

        match &vm_state.state {
            Some(state) => state.get_last_accepted_block_id().await,
            None => Err(Error::new(ErrorKind::NotFound, "state manager not found")),
        }
    }

    pub async fn get_account_from_state(&self, pubkey: Pubkey) -> io::Result<AccountSharedData> {
        let vm_state = self.state.read().await;
        // vm_state.get_account_from_state(pubkey).await;
        match &vm_state.state {
            Some(state) => state.get_account_shared_data(&pubkey).await,
            None => Err(Error::new(ErrorKind::NotFound, "state manager not found")),
        }
    }
}

pub(crate) fn setup_solana_logging() {
    #[rustfmt::skip]
    solana_logger::setup_with_default(
        "solana_rbpf::vm=debug,\
            solana_runtime::message_processor=debug,\
            solana_runtime::system_instruction_processor=trace",
    );
}

#[tonic::async_trait]
impl<A> CommonVm for Vm<A>
where
    A: AppSender + Send + Sync + Clone + 'static,
{
    type DatabaseManager = DatabaseManager;
    type AppSender = A;
    type ChainHandler = ChainHandler<ChainService<A>>;
    type StaticHandler = StaticHandler;
    type ValidatorState = ValidatorStateClient;

    async fn initialize(
        &mut self,
        ctx: Option<Context<Self::ValidatorState>>,
        _db_manager: BoxedDatabase,
        genesis_bytes: &[u8],
        _upgrade_bytes: &[u8],
        _config_bytes: &[u8],
        to_engine: Sender<snow::engine::common::message::Message>,
        _fxs: &[snow::engine::common::vm::Fx],
        app_sender: Self::AppSender,
    ) -> io::Result<()> {
        log::info!("initializingggggg Vm");
        setup_solana_logging();

        let mut vm_state = self.state.write().await;

        vm_state.ctx = ctx.clone();
        log::info!("post ctx");
        let version =
            Version::parse(VERSION).map_err(|e| Error::new(ErrorKind::Other, e.to_string()))?;
        vm_state.version = version;
        log::info!("post version");
        let genesis = Genesis::from_slice(genesis_bytes)?;
        vm_state.genesis = genesis.clone();
        log::info!("post geneiss");
        // @todo write genesis to state.
        let state = state::State::new(ctx.unwrap().chain_data_dir.clone());
        state.write_genesis(&genesis).await?;
        // let state = state::State {
        //     db: Arc::new(RwLock::new(db_manager)),
        //     verified_blocks: Arc::new(RwLock::new(HashMap::new())),
        // };
        vm_state.state = Some(state.clone());

        vm_state.to_engine = Some(to_engine);

        self.app_sender = Some(app_sender);

        let has_last_accepted = state.has_last_accepted_block().await?;
        if has_last_accepted {
            let last_accepted_blk_id = state.get_last_accepted_block_id().await?;
            vm_state.preferred = last_accepted_blk_id;
            log::info!("initialized Vm with last accepted block {last_accepted_blk_id}");
        } else {
            let mut genesis_block = Block::try_new(
                ids::Id::empty(),
                0,
                0,
                vec![],
                vec![],
                choices::status::Status::default(),
            )?;
            genesis_block.set_state(state.clone());
            genesis_block.accept().await?;

            let genesis_blk_id = genesis_block.id();
            vm_state.preferred = genesis_blk_id;
            log::info!("initialized Vm with genesis block {genesis_blk_id}");
        }

        self.mempool = Arc::new(RwLock::new(VecDeque::with_capacity(100)));

        log::info!("successfully initialized Vm");
        Ok(())
    }

    /// Called when the node is shutting down.
    async fn shutdown(&self) -> io::Result<()> {
        // grpc servers are shutdown via broadcast channel
        // if additional shutdown is required we can extend.
        Ok(())
    }

    async fn set_state(&self, snow_state: subnet::rpc::snow::State) -> io::Result<()> {
        self.set_state(snow_state).await
    }

    async fn version(&self) -> io::Result<String> {
        Ok(String::from(VERSION))
    }

    /// Creates static handlers.
    async fn create_static_handlers(
        &mut self,
    ) -> io::Result<HashMap<String, HttpHandler<Self::StaticHandler>>> {
        let handler = StaticHandler::new(StaticService::new());
        let mut handlers = HashMap::new();
        handlers.insert(
            "/static".to_string(),
            HttpHandler {
                lock_option: LockOptions::WriteLock,
                handler,
                server_addr: None,
            },
        );

        Ok(handlers)
    }

    /// Creates VM-specific handlers.
    async fn create_handlers(
        &mut self,
    ) -> io::Result<HashMap<String, HttpHandler<Self::ChainHandler>>> {
        let handler = ChainHandler::new(ChainService::new(self.clone()));
        let mut handlers = HashMap::new();
        handlers.insert(
            "/rpc".to_string(),
            HttpHandler {
                lock_option: LockOptions::WriteLock,
                handler,
                server_addr: None,
            },
        );

        Ok(handlers)
    }
}

#[tonic::async_trait]
impl<A> ChainVm for Vm<A>
where
    A: AppSender + Send + Sync + Clone + 'static,
{
    type Block = Block;

    /// Builds a block from mempool data.
    async fn build_block(&self) -> io::Result<<Self as ChainVm>::Block> {
        // check both the mempools.
        let mut mempool = self.mempool.write().await;
        let mut deploy_mempool = self.deploy_mempool.write().await;
        log::info!("build_block called for {} mempool", mempool.len());
        if mempool.is_empty() && deploy_mempool.is_empty() {
            return Err(Error::new(ErrorKind::Other, "no pending txs in mempool"));
        }
        // build block with both mempools.
        let vm_state = self.state.read().await;
        if let Some(state) = &vm_state.state {
            self.notify_block_ready().await;

            // "state" must have preferred block in cache/verified_block
            // otherwise, not found error from rpcchainvm database
            let prnt_blk = state.get_block(&vm_state.preferred).await?;
            let unix_now = Utc::now()
                .timestamp()
                .try_into()
                .expect("timestamp to convert from i64 to u64");
            let mut sanity_txs: Vec<SanitizedTransaction> = vec![];
            if mempool.is_empty() {
                log::info!("mempool is empty");
            } else {
                for mem_tx in mempool.pop_back() {
                    sanity_txs.push(mem_tx.clone());
                }
            }
            let mut deploy_txs: Vec<DeployTx> = vec![];
            if deploy_mempool.is_empty() {
                log::info!("deploy mempool is empty");
            } else {
                for deploy_tx in deploy_mempool.pop_back() {
                    let tx = deploy_tx.clone();
                    deploy_txs.push(tx);
                }
            }

            let mut block = Block::try_new(
                prnt_blk.id(),
                prnt_blk.height() + 1,
                unix_now,
                sanity_txs,
                deploy_txs,
                choices::status::Status::Processing,
            )?;
            block.set_state(state.clone());
            block.verify().await?;

            log::info!("successfully built block");
            return Ok(block);
        }

        Err(Error::new(ErrorKind::NotFound, "state manager not found"))
    }

    async fn set_preference(&self, id: ids::Id) -> io::Result<()> {
        let mut vm_state = self.state.write().await;
        vm_state.preferred = id;

        Ok(())
    }

    async fn last_accepted(&self) -> io::Result<ids::Id> {
        self.last_accepted().await
    }

    async fn issue_tx(&self) -> io::Result<<Self as ChainVm>::Block> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "issue_tx not implemented",
        ))
    }

    // Passes back ok as a no-op for now.
    // TODO: Remove after v1.11.x activates
    async fn verify_height_index(&self) -> io::Result<()> {
        Ok(())
    }

    // Returns an error as a no-op for now.
    async fn get_block_id_at_height(&self, _height: u64) -> io::Result<ids::Id> {
        Err(Error::new(ErrorKind::NotFound, "block id not found"))
    }

    async fn state_sync_enabled(&self) -> io::Result<bool> {
        Ok(false)
    }
}

#[tonic::async_trait]
impl<A> BatchedChainVm for Vm<A>
where
    A: Send + Sync + Clone + 'static,
{
    type Block = Block;

    async fn get_ancestors(
        &self,
        _block_id: ids::Id,
        _max_block_num: i32,
        _max_block_size: i32,
        _max_block_retrival_time: Duration,
    ) -> io::Result<Vec<Bytes>> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "get_ancestors not implemented",
        ))
    }
    async fn batched_parse_block(&self, _blocks: &[Vec<u8>]) -> io::Result<Vec<Self::Block>> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "batched_parse_block not implemented",
        ))
    }
}

#[tonic::async_trait]
impl<A> NetworkAppHandler for Vm<A>
where
    A: AppSender + Send + Sync + Clone + 'static,
{
    /// Currently, no app-specific messages, so returning Ok.
    async fn app_request(
        &self,
        _node_id: &ids::node::Id,
        _request_id: u32,
        _deadline: DateTime<Utc>,
        _request: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    /// Currently, no app-specific messages, so returning Ok.
    async fn app_request_failed(
        &self,
        _node_id: &ids::node::Id,
        _request_id: u32,
    ) -> io::Result<()> {
        Ok(())
    }

    /// Currently, no app-specific messages, so returning Ok.
    async fn app_response(
        &self,
        _node_id: &ids::node::Id,
        _request_id: u32,
        _response: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    /// Currently, no app-specific messages, so returning Ok.
    async fn app_gossip(&self, _node_id: &ids::node::Id, _msg: &[u8]) -> io::Result<()> {
        Ok(())
    }
}

#[tonic::async_trait]
impl<A> CrossChainAppHandler for Vm<A>
where
    A: AppSender + Send + Sync + Clone + 'static,
{
    /// Currently, no cross chain specific messages, so returning Ok.
    async fn cross_chain_app_request(
        &self,
        _chain_id: &ids::Id,
        _request_id: u32,
        _deadline: DateTime<Utc>,
        _request: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    /// Currently, no cross chain specific messages, so returning Ok.
    async fn cross_chain_app_request_failed(
        &self,
        _chain_id: &ids::Id,
        _request_id: u32,
    ) -> io::Result<()> {
        Ok(())
    }

    /// Currently, no cross chain specific messages, so returning Ok.
    async fn cross_chain_app_response(
        &self,
        _chain_id: &ids::Id,
        _request_id: u32,
        _response: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }
}

impl<A: AppSender> AppHandler for Vm<A> where A: AppSender + Send + Sync + Clone + 'static {}

#[tonic::async_trait]
impl<A> Connector for Vm<A>
where
    A: AppSender + Send + Sync + Clone + 'static,
{
    async fn connected(&self, _id: &ids::node::Id) -> io::Result<()> {
        // no-op
        Ok(())
    }

    async fn disconnected(&self, _id: &ids::node::Id) -> io::Result<()> {
        // no-op
        Ok(())
    }
}

#[tonic::async_trait]
impl<A> Checkable for Vm<A>
where
    A: AppSender + Send + Sync + Clone + 'static,
{
    async fn health_check(&self) -> io::Result<Vec<u8>> {
        Ok("200".as_bytes().to_vec())
    }
}

#[tonic::async_trait]
impl<A> Getter for Vm<A>
where
    A: AppSender + Send + Sync + Clone + 'static,
{
    type Block = Block;

    async fn get_block(&self, blk_id: ids::Id) -> io::Result<<Self as Getter>::Block> {
        let vm_state = self.state.read().await;
        if let Some(state) = &vm_state.state {
            let block = state.get_block(&blk_id).await?;
            return Ok(block);
        }

        Err(Error::new(ErrorKind::NotFound, "state manager not found"))
    }
}

#[tonic::async_trait]
impl<A> Parser for Vm<A>
where
    A: AppSender + Send + Sync + Clone + 'static,
{
    type Block = Block;

    async fn parse_block(&self, bytes: &[u8]) -> io::Result<<Self as Parser>::Block> {
        let vm_state = self.state.read().await;
        if let Some(state) = &vm_state.state {
            let mut new_block = Block::from_slice(bytes)?;
            new_block.set_status(choices::status::Status::Processing);
            new_block.set_state(state.clone());
            log::debug!("parsed block {}", new_block.id());

            match state.get_block(&new_block.id()).await {
                Ok(prev) => {
                    log::debug!("returning previously parsed block {}", prev.id());
                    return Ok(prev);
                }
                Err(_) => return Ok(new_block),
            };
        }

        Err(Error::new(ErrorKind::NotFound, "state manager not found"))
    }
}
