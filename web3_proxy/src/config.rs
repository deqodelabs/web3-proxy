use crate::rpcs::blockchain::BlockHashesCache;
use crate::rpcs::connection::Web3Connection;
use crate::rpcs::request::OpenRequestHandleMetrics;
use crate::{app::AnyhowJoinHandle, rpcs::blockchain::ArcBlock};
use argh::FromArgs;
use ethers::prelude::TxHash;
use hashbrown::HashMap;
use log::warn;
use migration::sea_orm::DatabaseConnection;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::broadcast;

pub type BlockAndRpc = (Option<ArcBlock>, Arc<Web3Connection>);
pub type TxHashAndRpc = (TxHash, Arc<Web3Connection>);

#[derive(Debug, FromArgs)]
/// Web3_proxy is a fast caching and load balancing proxy for web3 (Ethereum or similar) JsonRPC servers.
pub struct CliConfig {
    /// path to a toml of rpc servers
    #[argh(option, default = "\"./config/development.toml\".to_string()")]
    pub config: String,

    /// what port the proxy should listen on
    #[argh(option, default = "8544")]
    pub port: u16,

    /// what port the proxy should expose prometheus stats on
    #[argh(option, default = "8543")]
    pub prometheus_port: u16,

    /// number of worker threads. Defaults to the number of logical processors
    #[argh(option, default = "0")]
    pub workers: usize,

    /// path to a binary file used to encrypt cookies. Should be at least 64 bytes.
    #[argh(option, default = "\"./data/development_cookie_key\".to_string()")]
    pub cookie_key_filename: String,
}

#[derive(Debug, Deserialize)]
pub struct TopConfig {
    pub app: AppConfig,
    pub balanced_rpcs: HashMap<String, Web3ConnectionConfig>,
    // TODO: instead of an option, give it a default
    pub private_rpcs: Option<HashMap<String, Web3ConnectionConfig>>,
    /// unknown config options get put here
    #[serde(flatten, default = "HashMap::default")]
    pub extra: HashMap<String, serde_json::Value>,
}

/// shared configuration between Web3Connections
// TODO: no String, only &str
#[derive(Debug, Default, Deserialize)]
pub struct AppConfig {
    /// Request limit for allowed origins for anonymous users.
    /// These requests get rate limited by IP.
    #[serde(default = "default_allowed_origin_requests_per_period")]
    pub allowed_origin_requests_per_period: HashMap<String, u64>,

    /// EVM chain id. 1 for ETH
    /// TODO: better type for chain_id? max of `u64::MAX / 2 - 36` <https://github.com/ethereum/EIPs/issues/2294>
    pub chain_id: u64,

    /// Database is used for user data.
    /// Currently supports mysql or compatible backend.
    pub db_url: Option<String>,

    /// minimum size of the connection pool for the database.
    /// If none, the number of workers are used.
    pub db_min_connections: Option<u32>,

    /// maximum size of the connection pool for the database.
    /// If none, the minimum * 2 is used.
    pub db_max_connections: Option<u32>,

    /// Read-only replica of db_url.
    pub db_replica_url: Option<String>,

    /// minimum size of the connection pool for the database replica.
    /// If none, db_min_connections is used.
    pub db_replica_min_connections: Option<u32>,

    /// maximum size of the connection pool for the database replica.
    /// If none, db_max_connections is used.
    pub db_replica_max_connections: Option<u32>,

    /// Default request limit for registered users.
    /// 0 = block all requests
    /// None = allow all requests
    pub default_user_max_requests_per_period: Option<u64>,

    /// Restrict user registration.
    /// None = no code needed
    pub invite_code: Option<String>,
    pub login_domain: Option<String>,

    /// Rate limit for bearer token authenticated entrypoints.
    /// This is separate from the rpc limits.
    #[serde(default = "default_bearer_token_max_concurrent_requests")]
    pub bearer_token_max_concurrent_requests: u64,

    /// Rate limit for the login entrypoint.
    /// This is separate from the rpc limits.
    #[serde(default = "default_login_rate_limit_per_period")]
    pub login_rate_limit_per_period: u64,

    /// The soft limit prevents thundering herds as new blocks are seen.
    #[serde(default = "default_min_sum_soft_limit")]
    pub min_sum_soft_limit: u32,

    /// Another knob for preventing thundering herds as new blocks are seen.
    #[serde(default = "default_min_synced_rpcs")]
    pub min_synced_rpcs: usize,

    /// Concurrent request limit for anonymous users.
    /// Some(0) = block all requests
    /// None = allow all requests
    pub public_max_concurrent_requests: Option<usize>,

    /// Request limit for anonymous users.
    /// Some(0) = block all requests
    /// None = allow all requests
    pub public_requests_per_period: Option<u64>,

    /// Salt for hashing recent ips
    pub public_recent_ips_salt: Option<String>,

    /// RPC responses are cached locally
    #[serde(default = "default_response_cache_max_bytes")]
    pub response_cache_max_bytes: usize,

    /// the stats page url for an anonymous user.
    pub redirect_public_url: Option<String>,

    /// the stats page url for a logged in user. if set, must contain "{rpc_key_id}"
    pub redirect_rpc_key_url: Option<String>,

    /// Optionally send errors to <https://sentry.io>
    pub sentry_url: Option<String>,

    /// Track rate limits in a redis (or compatible backend)
    /// It is okay if this data is lost.
    pub volatile_redis_url: Option<String>,

    /// maximum size of the connection pool for the cache
    /// If none, the minimum * 2 is used
    pub volatile_redis_max_connections: Option<usize>,

    /// unknown config options get put here
    #[serde(flatten, default = "HashMap::default")]
    pub extra: HashMap<String, serde_json::Value>,
}

fn default_allowed_origin_requests_per_period() -> HashMap<String, u64> {
    HashMap::new()
}

/// This might cause a thundering herd!
fn default_min_sum_soft_limit() -> u32 {
    1
}

/// Only require 1 server. This might cause a thundering herd!
fn default_min_synced_rpcs() -> usize {
    1
}

/// Having a low amount of concurrent requests for bearer tokens keeps us from hammering the database.
fn default_bearer_token_max_concurrent_requests() -> u64 {
    2
}

/// Having a low amount of requests per period (usually minute) for login is safest.
fn default_login_rate_limit_per_period() -> u64 {
    10
}

fn default_response_cache_max_bytes() -> usize {
    // TODO: default to some percentage of the system?
    // 100 megabytes
    10_usize.pow(8)
}

/// Configuration for a backend web3 RPC server
#[derive(Debug, Deserialize)]
pub struct Web3ConnectionConfig {
    /// simple way to disable a connection without deleting the row
    #[serde(default)]
    pub disabled: bool,
    /// a name used in /status and other user facing messages
    pub display_name: Option<String>,
    /// websocket (or http if no websocket)
    pub url: String,
    /// block data limit. If None, will be queried
    pub block_data_limit: Option<u64>,
    /// the requests per second at which the server starts slowing down
    pub soft_limit: u32,
    /// the requests per second at which the server throws errors (rate limit or otherwise)
    pub hard_limit: Option<u64>,
    /// All else equal, a server with a lower tier receives all requests
    #[serde(default = "default_tier")]
    pub tier: u64,
    /// Subscribe to the firehose of pending transactions
    /// Don't do this with free rpcs
    #[serde(default)]
    pub subscribe_txs: Option<bool>,
    /// unknown config options get put here
    #[serde(flatten, default = "HashMap::default")]
    pub extra: HashMap<String, serde_json::Value>,
}

fn default_tier() -> u64 {
    0
}

impl Web3ConnectionConfig {
    /// Create a Web3Connection from config
    /// TODO: move this into Web3Connection? (just need to make things pub(crate))
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn(
        self,
        name: String,
        allowed_lag: u64,
        db_conn: Option<DatabaseConnection>,
        redis_pool: Option<redis_rate_limiter::RedisPool>,
        chain_id: u64,
        http_client: Option<reqwest::Client>,
        http_interval_sender: Option<Arc<broadcast::Sender<()>>>,
        block_map: BlockHashesCache,
        block_sender: Option<flume::Sender<BlockAndRpc>>,
        tx_id_sender: Option<flume::Sender<TxHashAndRpc>>,
        open_request_handle_metrics: Arc<OpenRequestHandleMetrics>,
    ) -> anyhow::Result<(Arc<Web3Connection>, AnyhowJoinHandle<()>)> {
        if !self.extra.is_empty() {
            warn!(
                "unknown Web3ConnectionConfig fields!: {:?}",
                self.extra.keys()
            );
        }

        let hard_limit = match (self.hard_limit, redis_pool) {
            (None, None) => None,
            (Some(hard_limit), Some(redis_client_pool)) => Some((hard_limit, redis_client_pool)),
            (None, Some(_)) => None,
            (Some(_hard_limit), None) => {
                return Err(anyhow::anyhow!(
                    "no redis client pool! needed for hard limit"
                ))
            }
        };

        let tx_id_sender = if self.subscribe_txs.unwrap_or(false) {
            tx_id_sender
        } else {
            None
        };

        Web3Connection::spawn(
            name,
            allowed_lag,
            self.display_name,
            chain_id,
            db_conn,
            self.url,
            http_client,
            http_interval_sender,
            hard_limit,
            self.soft_limit,
            self.block_data_limit,
            block_map,
            block_sender,
            tx_id_sender,
            true,
            self.tier,
            open_request_handle_metrics,
        )
        .await
    }
}
