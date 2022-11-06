use std::{
    io::{self, Error, ErrorKind},
    sync::Arc,
};

use crate::block::Block;
use avalanche_types::{choices, ids, subnet};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// Represents persistent block and chain states for Vm.
/// TODO: use cache for optimization
#[derive(Clone)]
pub struct State {
    pub db: Arc<RwLock<Box<dyn subnet::rpc::database::Database + Send + Sync>>>,
}

impl Default for State {
    fn default() -> State {
        Self {
            db: Arc::new(RwLock::new(subnet::rpc::database::memdb::Database::new())),
        }
    }
}

const LAST_ACCEPTED_BLOCK_KEY: &[u8] = b"last_accepted_block";

const BLOCK_WITH_STATUS_PREFIX: u8 = 0x0;

const DELIMITER: u8 = b'/';

fn block_with_status_key(blk_id: &ids::Id) -> Vec<u8> {
    let mut k: Vec<u8> = Vec::with_capacity(ids::LEN + 2);
    k.push(BLOCK_WITH_STATUS_PREFIX);
    k.push(DELIMITER);
    k.extend_from_slice(&blk_id.to_vec());
    k
}

#[derive(Serialize, Deserialize, Clone)]
struct BlockWithStatus {
    block_bytes: Vec<u8>,
    status: choices::status::Status,
}

impl BlockWithStatus {
    fn encode(&self) -> io::Result<Vec<u8>> {
        serde_json::to_vec(&self).map_err(|e| {
            Error::new(
                ErrorKind::Other,
                format!("failed to serialize BlockStatus to JSON bytes {}", e),
            )
        })
    }

    fn from_slice(d: impl AsRef<[u8]>) -> io::Result<Self> {
        let dd = d.as_ref();
        serde_json::from_slice(dd).map_err(|e| {
            Error::new(
                ErrorKind::Other,
                format!("failed to deserialize BlockStatus from JSON {}", e),
            )
        })
    }
}

impl State {
    pub async fn set_last_accepted_block(&self, blk_id: &ids::Id) -> io::Result<()> {
        let mut db = self.db.write().await;
        db.put(LAST_ACCEPTED_BLOCK_KEY, &blk_id.to_vec())
            .await
            .map_err(|e| {
                Error::new(
                    ErrorKind::Other,
                    format!("failed to put last accepted block: {:?}", e),
                )
            })
    }

    pub async fn has_last_accepted_block(&self) -> io::Result<bool> {
        let db = self.db.read().await;
        match db.has(LAST_ACCEPTED_BLOCK_KEY).await {
            Ok(found) => Ok(found),
            Err(e) => Err(Error::new(
                ErrorKind::Other,
                format!("failed to load last accepted block {}", e),
            )),
        }
    }

    pub async fn get_last_accepted_block_id(&self) -> io::Result<ids::Id> {
        let db = self.db.read().await;
        match db.get(LAST_ACCEPTED_BLOCK_KEY).await {
            Ok(d) => Ok(ids::Id::from_slice(&d)),
            Err(e) => {
                if e.kind() == ErrorKind::NotFound && e.to_string().contains("not found") {
                    return Ok(ids::Id::empty());
                }
                return Err(e);
            }
        }
    }

    pub async fn put_block(&mut self, block: &Block) -> io::Result<()> {
        let blk_id = block.id();
        let blk_bytes = block.to_slice()?;

        let mut db = self.db.write().await;

        let blk_status = BlockWithStatus {
            block_bytes: blk_bytes,
            status: block.status(),
        };
        let blk_status_bytes = blk_status.encode()?;

        db.put(&block_with_status_key(&blk_id), &blk_status_bytes)
            .await
            .map_err(|e| Error::new(ErrorKind::Other, format!("failed to put block: {:?}", e)))
    }

    pub async fn get_block(&self, blk_id: &ids::Id) -> io::Result<Block> {
        let db = self.db.read().await;

        let blk_status_bytes = db.get(&block_with_status_key(blk_id)).await?;
        let blk_status = BlockWithStatus::from_slice(&blk_status_bytes)?;

        let mut blk = Block::from_slice(&blk_status.block_bytes)?;
        blk.set_status(blk_status.status);

        Ok(blk)
    }
}

/// RUST_LOG=debug cargo test --package timestampvm --lib -- state::test_state --exact --show-output
#[tokio::test]
async fn test_state() {
    let _ = env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .is_test(true)
        .try_init();

    let genesis_blk = Block::new(
        ids::Id::empty(),
        0,
        random_manager::u64(),
        random_manager::bytes(10).unwrap(),
        choices::status::Status::Accepted,
    )
    .unwrap();
    log::info!("genesis block: {genesis_blk}");

    let blk1 = Block::new(
        genesis_blk.id(),
        1,
        genesis_blk.timestamp() + 1,
        random_manager::bytes(10).unwrap(),
        choices::status::Status::Accepted,
    )
    .unwrap();
    log::info!("blk1: {blk1}");

    let mut state = State::default();
    assert!(!state.has_last_accepted_block().await.unwrap());

    state.put_block(&genesis_blk).await.unwrap();
    assert!(!state.has_last_accepted_block().await.unwrap());

    state.put_block(&blk1).await.unwrap();
    state.set_last_accepted_block(&blk1.id()).await.unwrap();
    assert!(state.has_last_accepted_block().await.unwrap());

    let last_accepted_blk_id = state.get_last_accepted_block_id().await.unwrap();
    assert_eq!(last_accepted_blk_id, blk1.id());

    let read_blk = state.get_block(&genesis_blk.id()).await.unwrap();
    assert_eq!(genesis_blk, read_blk);

    let read_blk = state.get_block(&blk1.id()).await.unwrap();
    assert_eq!(blk1, read_blk);
}