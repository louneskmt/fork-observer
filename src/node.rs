use std::cmp::max;
use std::fmt;

use crate::error::{FetchError, JsonRPCError};
use crate::types::{ChainTip, ChainTipStatus, HeaderInfo, Tree};

use bitcoincore_rpc::bitcoin;
use bitcoincore_rpc::bitcoin::{BlockHash, BlockHeader};
use bitcoincore_rpc::Auth;
use bitcoincore_rpc::Client;
use bitcoincore_rpc::RpcApi;

use async_trait::async_trait;

use log::{debug, error, warn};

use tokio::task;

const BTCD_USE_REST: bool = false;

#[async_trait]
pub trait Node: Sync {
    fn info(&self) -> NodeInfo;
    fn use_rest(&self) -> bool;
    fn rpc_url(&self) -> String;
    async fn version(&self) -> Result<String, FetchError>;
    async fn block_header(&self, hash: &BlockHash) -> Result<BlockHeader, FetchError>;
    async fn block_hash(&self, height: u64) -> Result<BlockHash, FetchError>;
    async fn tips(&self) -> Result<Vec<ChainTip>, FetchError>;

    async fn new_headers(
        &self,
        tips: &Vec<ChainTip>,
        tree: &Tree,
        min_fork_height: u64,
    ) -> Result<Vec<HeaderInfo>, FetchError> {
        let mut new_headers: Vec<HeaderInfo> = Vec::new();

        let mut active_new_headers: Vec<HeaderInfo> =
            self.new_active_headers(tips, tree, min_fork_height).await?;
        new_headers.append(&mut active_new_headers);
        let mut nonactive_new_headers: Vec<HeaderInfo> = self
            .new_nonactive_headers(tips, tree, min_fork_height)
            .await?;
        new_headers.append(&mut nonactive_new_headers);
        Ok(new_headers)
    }

    async fn new_active_headers(
        &self,
        tips: &Vec<ChainTip>,
        tree: &Tree,
        min_fork_height: u64,
    ) -> Result<Vec<HeaderInfo>, FetchError> {
        let mut new_headers: Vec<HeaderInfo> = Vec::new();
        let first_fork_tip = match tips
            .iter()
            .filter(|tip| tip.height - tip.branchlen as u64 > min_fork_height)
            .min_by_key(|tip| tip.height - tip.branchlen as u64)
        {
            Some(tip) => tip,
            None => {
                warn!("No tip qualifies as first_fork_tip. Is min_fork_height={} reasonable for this network?", min_fork_height);
                return Ok(new_headers);
            }
        };
        let min_height = first_fork_tip.height - first_fork_tip.branchlen as u64;
        let scan_start_height = max(min_height as i64 - 5, 0) as u64;

        let current_height: u64;
        {
            let locked_tree = tree.lock().await;
            if locked_tree.0.node_count() == 0 {
                current_height = scan_start_height;
            } else {
                let max_tip_idx = locked_tree
                .0
                .externals(petgraph::Direction::Outgoing)
                .max_by_key(|idx| locked_tree.0[*idx].height)
                .expect("we have at least one node in the tree so we should also have a max height node in the tree");
                current_height = locked_tree.0[max_tip_idx].height;
            }
        }

        let active_tip = match tips
            .iter()
            .filter(|tip| tip.status == ChainTipStatus::Active)
            .last()
        {
            Some(active_tip) => active_tip,
            None => {
                return Err(FetchError::DataError(String::from(
                    "No 'active' chain tip returned",
                )))
            }
        };

        if self.use_rest() {
            let mut headers: Vec<bitcoin::BlockHeader>;
            const STEP_SIZE: u64 = 2000;
            for query_height in (current_height + 1..=active_tip.height).step_by(STEP_SIZE as usize)
            {
                let header_hash = self.block_hash(query_height).await?;
                {
                    let locked_tree = tree.lock().await;
                    if locked_tree.1.contains_key(&header_hash) {
                        continue;
                    }
                }
                headers = self
                    .active_chain_headers_rest(STEP_SIZE, header_hash)
                    .await?;
                for height_header_pair in
                    (query_height..(query_height + headers.len() as u64)).zip(headers)
                {
                    new_headers.push(HeaderInfo {
                        height: height_header_pair.0,
                        header: height_header_pair.1,
                    });
                }
            }
        } else {
            for height in current_height + 1..=active_tip.height {
                let header_hash = self.block_hash(height).await?;
                {
                    let locked_tree = tree.lock().await;
                    if locked_tree.1.contains_key(&header_hash) {
                        continue;
                    }
                }
                let header = self.block_header(&header_hash).await?;
                new_headers.push(HeaderInfo { height, header });
            }
        }

        Ok(new_headers)
    }

    async fn new_nonactive_headers(
        &self,
        tips: &Vec<ChainTip>,
        tree: &Tree,
        min_fork_height: u64,
    ) -> Result<Vec<HeaderInfo>, FetchError> {
        let mut new_headers: Vec<HeaderInfo> = Vec::new();

        for inactive_tip in tips
            .iter()
            .filter(|tip| tip.height - tip.branchlen as u64 > min_fork_height)
            .filter(|tip| tip.status != ChainTipStatus::Active)
        {
            let mut next_header = inactive_tip.block_hash();
            for i in 0..=inactive_tip.branchlen {
                {
                    let tree_locked = tree.lock().await;
                    if tree_locked.1.contains_key(&inactive_tip.block_hash()) {
                        break;
                    }
                }

                let height = inactive_tip.height - i as u64;
                debug!(
                    "loading non-active-chain header: hash={}, height={}",
                    next_header, height
                );

                let header = self.block_header(&next_header).await?;

                new_headers.push(HeaderInfo { height, header });
                next_header = header.prev_blockhash;
            }
        }

        Ok(new_headers)
    }

    async fn active_chain_headers_rest(
        &self,
        count: u64,
        start: BlockHash,
    ) -> Result<Vec<bitcoin::BlockHeader>, FetchError> {
        assert!(self.use_rest());
        debug!(
            "loading active-chain headers starting from {}",
            start.to_string()
        );

        let url = format!(
            "http://{}/rest/headers/{}/{}.bin",
            self.rpc_url(),
            count,
            start
        );
        let res = minreq::get(url.clone()).with_timeout(8).send()?;

        if res.status_code != 200 {
            return Err(FetchError::BitcoinCoreREST(format!(
                "could not load headers from REST URL ({}): {} {}: {:?}",
                url,
                res.status_code,
                res.reason_phrase,
                res.as_str(),
            )));
        }

        let header_results: Result<
            Vec<bitcoin::BlockHeader>,
            bitcoincore_rpc::bitcoin::consensus::encode::Error,
        > = res
            .as_bytes()
            .chunks(80)
            .map(bitcoin::consensus::deserialize::<bitcoin::BlockHeader>)
            .collect();

        let headers = match header_results {
            Ok(headers) => headers,
            Err(e) => {
                return Err(FetchError::BitcoinCoreREST(format!(
                    "could not deserialize REST header response: {}",
                    e
                )))
            }
        };

        debug!(
            "loaded {} active-chain headers starting from {}",
            headers.len(),
            start.to_string()
        );

        Ok(headers)
    }
}

#[derive(Hash, Clone)]
pub struct NodeInfo {
    pub id: u8,
    pub name: String,
    pub description: String,
}

impl fmt::Display for NodeInfo {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Node(id={}, name='{}', description='{}')",
            self.id, self.name, self.description
        )
    }
}

#[derive(Hash, Clone)]
pub struct BitcoinCoreNode {
    info: NodeInfo,
    rpc_url: String,
    rpc_auth: Auth,
    use_rest: bool,
}

impl BitcoinCoreNode {
    pub fn new(info: NodeInfo, rpc_url: String, rpc_auth: Auth, use_rest: bool) -> Self {
        BitcoinCoreNode {
            info,
            rpc_url,
            rpc_auth,
            use_rest,
        }
    }

    fn rpc_client(&self) -> Result<Client, FetchError> {
        match Client::new(&self.rpc_url, self.rpc_auth.clone()) {
            Ok(c) => Ok(c),
            Err(e) => {
                error!(
                    "Could not create a RPC client for node {}: {:?}",
                    self.info(),
                    e
                );
                Err(FetchError::from(e))
            }
        }
    }
}

#[async_trait]
impl Node for BitcoinCoreNode {
    fn info(&self) -> NodeInfo {
        self.info.clone()
    }

    fn use_rest(&self) -> bool {
        self.use_rest
    }

    fn rpc_url(&self) -> String {
        self.rpc_url.clone()
    }

    async fn version(&self) -> Result<String, FetchError> {
        let rpc = self.rpc_client()?;
        match task::spawn_blocking(move || rpc.get_network_info()).await {
            Ok(result) => match result {
                Ok(result) => Ok(result.subversion),
                Err(e) => Err(e.into()),
            },
            Err(e) => Err(e.into()),
        }
    }

    async fn block_hash(&self, height: u64) -> Result<BlockHash, FetchError> {
        let rpc = self.rpc_client()?;
        match task::spawn_blocking(move || rpc.get_block_hash(height)).await {
            Ok(result) => match result {
                Ok(result) => Ok(result),
                Err(e) => Err(e.into()),
            },
            Err(e) => Err(e.into()),
        }
    }

    async fn block_header(&self, hash: &BlockHash) -> Result<BlockHeader, FetchError> {
        let rpc = self.rpc_client()?;
        let hash_clone = hash.clone();
        match task::spawn_blocking(move || rpc.get_block_header(&hash_clone)).await {
            Ok(result) => match result {
                Ok(result) => Ok(result),
                Err(e) => Err(e.into()),
            },
            Err(e) => Err(e.into()),
        }
    }

    async fn tips(&self) -> Result<Vec<ChainTip>, FetchError> {
        let rpc = self.rpc_client()?;
        match task::spawn_blocking(move || rpc.get_chain_tips()).await {
            Ok(tips_result) => match tips_result {
                Ok(tips) => Ok(tips.iter().map(|t| t.clone().into()).collect()),
                Err(e) => Err(e.into()),
            },
            Err(e) => Err(e.into()),
        }
    }
}

#[derive(Hash, Clone)]
pub struct BtcdNode {
    info: NodeInfo,
    rpc_url: String,
    rpc_user: String,
    rpc_password: String,
}

impl BtcdNode {
    pub fn new(info: NodeInfo, rpc_url: String, rpc_user: String, rpc_password: String) -> Self {
        BtcdNode {
            info,
            rpc_url,
            rpc_user,
            rpc_password,
        }
    }
}

#[async_trait]
impl Node for BtcdNode {
    fn info(&self) -> NodeInfo {
        self.info.clone()
    }

    fn use_rest(&self) -> bool {
        BTCD_USE_REST
    }

    fn rpc_url(&self) -> String {
        self.rpc_url.clone()
    }

    async fn version(&self) -> Result<String, FetchError> {
        Err(FetchError::BtcdRPC(JsonRPCError::NotImplemented))
    }

    async fn block_header(&self, hash: &BlockHash) -> Result<BlockHeader, FetchError> {
        let url = format!("http://{}/", self.rpc_url);
        match crate::jsonrpc::btcd_blockheader(
            url,
            self.rpc_user.clone(),
            self.rpc_password.clone(),
            hash.to_string(),
        ) {
            Ok(header) => Ok(header),
            Err(error) => Err(FetchError::BtcdRPC(error)),
        }
    }

    async fn block_hash(&self, height: u64) -> Result<BlockHash, FetchError> {
        let url = format!("http://{}/", self.rpc_url);
        match crate::jsonrpc::btcd_blockhash(
            url,
            self.rpc_user.clone(),
            self.rpc_password.clone(),
            height,
        ) {
            Ok(tips) => Ok(tips),
            Err(error) => Err(FetchError::BtcdRPC(error)),
        }
    }

    async fn tips(&self) -> Result<Vec<ChainTip>, FetchError> {
        let url = format!("http://{}/", self.rpc_url);
        match crate::jsonrpc::btcd_chaintips(url, self.rpc_user.clone(), self.rpc_password.clone())
        {
            Ok(tips) => Ok(tips),
            Err(error) => Err(FetchError::BtcdRPC(error)),
        }
    }
}