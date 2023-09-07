use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures::lock::{OwnedMutexGuard, OwnedMutexLockFuture};
use futures::{FutureExt, TryFutureExt};
use monero_serai::rpc::{HttpRpc, RpcConnection, RpcError};
use serde::Deserialize;
use serde_json::json;

use cuprate_common::BlockID;

use crate::pow::BlockPOWInfo;
use crate::{DatabaseRequest, DatabaseResponse};

enum RpcState<R: RpcConnection> {
    Locked,
    Acquiring(OwnedMutexLockFuture<monero_serai::rpc::Rpc<R>>),
    Acquired(OwnedMutexGuard<monero_serai::rpc::Rpc<R>>),
}
pub struct Rpc<R: RpcConnection> {
    rpc: Arc<futures::lock::Mutex<monero_serai::rpc::Rpc<R>>>,
    rpc_state: RpcState<R>,
    error_slot: Arc<Mutex<Option<RpcError>>>,
}

impl Rpc<HttpRpc> {
    pub fn new_http(addr: String) -> Rpc<HttpRpc> {
        let http_rpc = HttpRpc::new(addr).unwrap();
        Rpc {
            rpc: Arc::new(futures::lock::Mutex::new(http_rpc)),
            rpc_state: RpcState::Locked,
            error_slot: Arc::new(Mutex::new(None)),
        }
    }
}

impl<R: RpcConnection> Clone for Rpc<R> {
    fn clone(&self) -> Self {
        Rpc {
            rpc: Arc::clone(&self.rpc),
            rpc_state: RpcState::Locked,
            error_slot: Arc::clone(&self.error_slot),
        }
    }
}

impl<R: RpcConnection + Send + Sync + 'static> tower::Service<DatabaseRequest> for Rpc<R> {
    type Response = DatabaseResponse;
    type Error = tower::BoxError;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if let Some(rpc_error) = self.error_slot.lock().unwrap().clone() {
            return Poll::Ready(Err(rpc_error.into()));
        }
        loop {
            match &mut self.rpc_state {
                RpcState::Locked => {
                    self.rpc_state = RpcState::Acquiring(Arc::clone(&self.rpc).lock_owned())
                }
                RpcState::Acquiring(rpc) => {
                    self.rpc_state = RpcState::Acquired(futures::ready!(rpc.poll_unpin(cx)))
                }
                RpcState::Acquired(_) => return Poll::Ready(Ok(())),
            }
        }
    }

    fn call(&mut self, req: DatabaseRequest) -> Self::Future {
        let RpcState::Acquired(rpc) = std::mem::replace(&mut self.rpc_state, RpcState::Locked)
        else {
            panic!("poll_ready was not called first!");
        };

        let err_slot = self.error_slot.clone();

        match req {
            DatabaseRequest::ChainHeight => async move {
                let res: Result<_, RpcError> = rpc
                    .get_height()
                    .map_ok(|height| DatabaseResponse::ChainHeight(height.try_into().unwrap()))
                    .await;
                if let Err(e) = &res {
                    *err_slot.lock().unwrap() = Some(e.clone());
                }
                res.map_err(Into::into)
            }
            .boxed(),

            DatabaseRequest::BlockHeader(id) => match id {
                BlockID::Hash(hash) => async move {
                    let res: Result<_, RpcError> = rpc
                        .get_block(hash)
                        .map_ok(|block| DatabaseResponse::BlockHeader(block.header))
                        .await;
                    if let Err(e) = &res {
                        *err_slot.lock().unwrap() = Some(e.clone());
                    }
                    res.map_err(Into::into)
                }
                .boxed(),
                BlockID::Height(height) => async move {
                    let res: Result<_, RpcError> = rpc
                        .get_block_by_number(height.try_into().unwrap())
                        .map_ok(|block| DatabaseResponse::BlockHeader(block.header))
                        .await;
                    if let Err(e) = &res {
                        *err_slot.lock().unwrap() = Some(e.clone());
                    }
                    res.map_err(Into::into)
                }
                .boxed(),
            },
            DatabaseRequest::BlockPOWInfo(id) => get_blocks_pow_info(id, rpc).boxed(),
        }
    }
}

async fn get_blocks_pow_info<R: RpcConnection>(
    id: BlockID,
    rpc: OwnedMutexGuard<monero_serai::rpc::Rpc<R>>,
) -> Result<DatabaseResponse, tower::BoxError> {
    #[derive(Deserialize, Debug)]
    struct BlockHeaderResponse {
        cumulative_difficulty: u64,
        cumulative_difficulty_top64: u64,
        timestamp: u64,
    }

    #[derive(Deserialize, Debug)]
    struct Response {
        block_header: BlockHeaderResponse,
    }

    match id {
        BlockID::Height(height) => {
            let res = rpc
                .json_rpc_call::<Response>(
                    "get_block_header_by_height",
                    Some(json!({"height": height})),
                )
                .await?;
            Ok(DatabaseResponse::BlockPOWInfo(BlockPOWInfo {
                timestamp: res.block_header.timestamp,
                cumulative_difficulty: u128_from_low_high(
                    res.block_header.cumulative_difficulty,
                    res.block_header.cumulative_difficulty_top64,
                ),
            }))
        }
        BlockID::Hash(hash) => {
            let res = rpc
                .json_rpc_call::<Response>("get_block_header_by_hash", Some(json!({"hash": hash})))
                .await?;
            Ok(DatabaseResponse::BlockPOWInfo(BlockPOWInfo {
                timestamp: res.block_header.timestamp,
                cumulative_difficulty: u128_from_low_high(
                    res.block_header.cumulative_difficulty,
                    res.block_header.cumulative_difficulty_top64,
                ),
            }))
        }
    }
}

fn u128_from_low_high(low: u64, high: u64) -> u128 {
    let res: u128 = high as u128;
    res << 64 | low as u128
}
