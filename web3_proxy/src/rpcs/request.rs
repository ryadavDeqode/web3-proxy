use super::one::Web3Rpc;
use super::provider::Web3Provider;
use crate::frontend::authorization::Authorization;
use anyhow::Context;
use chrono::Utc;
use entities::revert_log;
use entities::sea_orm_active_enums::Method;
use ethers::providers::{HttpClientError, ProviderError, WsClientError};
use ethers::types::{Address, Bytes};
use log::{debug, error, trace, warn, Level};
use migration::sea_orm::{self, ActiveEnum, ActiveModelTrait};
use serde_json::json;
use std::fmt;
use std::sync::Arc;
use thread_fast_rng::rand::Rng;
use tokio::time::{sleep, Duration, Instant};

#[derive(Debug)]
pub enum OpenRequestResult {
    Handle(OpenRequestHandle),
    /// Unable to start a request. Retry at the given time.
    RetryAt(Instant),
    /// Unable to start a request because the server is not synced
    /// contains "true" if backup servers were attempted
    NotReady(bool),
}

/// Make RPC requests through this handle and drop it when you are done.
/// Opening this handle checks rate limits. Developers, try to keep opening a handle and using it as close together as possible
#[derive(Debug)]
pub struct OpenRequestHandle {
    authorization: Arc<Authorization>,
    conn: Arc<Web3Rpc>,
}

/// Depending on the context, RPC errors can require different handling.
pub enum RequestRevertHandler {
    /// Log at the trace level. Use when errors are expected.
    TraceLevel,
    /// Log at the debug level. Use when errors are expected.
    DebugLevel,
    /// Log at the error level. Use when errors are bad.
    ErrorLevel,
    /// Log at the warn level. Use when errors do not cause problems.
    WarnLevel,
    /// Potentially save the revert. Users can tune how often this happens
    Save,
}

// TODO: second param could be skipped since we don't need it here
#[derive(serde::Deserialize, serde::Serialize)]
struct EthCallParams((EthCallFirstParams, Option<serde_json::Value>));

#[derive(serde::Deserialize, serde::Serialize)]
struct EthCallFirstParams {
    to: Address,
    data: Option<Bytes>,
}

impl From<Level> for RequestRevertHandler {
    fn from(level: Level) -> Self {
        match level {
            Level::Trace => RequestRevertHandler::TraceLevel,
            Level::Debug => RequestRevertHandler::DebugLevel,
            Level::Error => RequestRevertHandler::ErrorLevel,
            Level::Warn => RequestRevertHandler::WarnLevel,
            _ => unimplemented!("unexpected tracing Level"),
        }
    }
}

impl Authorization {
    /// Save a RPC call that return "execution reverted" to the database.
    async fn save_revert(
        self: Arc<Self>,
        method: Method,
        params: EthCallFirstParams,
    ) -> anyhow::Result<()> {
        let rpc_key_id = match self.checks.rpc_secret_key_id {
            Some(rpc_key_id) => rpc_key_id.into(),
            None => {
                // // trace!(?self, "cannot save revert without rpc_key_id");
                return Ok(());
            }
        };

        let db_conn = self.db_conn.as_ref().context("no database connection")?;

        // TODO: should the database set the timestamp?
        // we intentionally use "now" and not the time the request started
        // why? because we aggregate stats and setting one in the past could cause confusion
        let timestamp = Utc::now();
        let to: Vec<u8> = params
            .to
            .as_bytes()
            .try_into()
            .expect("address should always convert to a Vec<u8>");
        let call_data = params.data.map(|x| format!("{}", x));

        let rl = revert_log::ActiveModel {
            rpc_key_id: sea_orm::Set(rpc_key_id),
            method: sea_orm::Set(method),
            to: sea_orm::Set(to),
            call_data: sea_orm::Set(call_data),
            timestamp: sea_orm::Set(timestamp),
            ..Default::default()
        };

        let rl = rl
            .save(db_conn)
            .await
            .context("Failed saving new revert log")?;

        // TODO: what log level?
        // TODO: better format
        trace!("revert_log: {:?}", rl);

        // TODO: return something useful
        Ok(())
    }
}

impl OpenRequestHandle {
    pub async fn new(authorization: Arc<Authorization>, conn: Arc<Web3Rpc>) -> Self {
        Self {
            authorization,
            conn,
        }
    }

    pub fn connection_name(&self) -> String {
        self.conn.name.clone()
    }

    #[inline]
    pub fn clone_connection(&self) -> Arc<Web3Rpc> {
        self.conn.clone()
    }

    /// Send a web3 request
    /// By having the request method here, we ensure that the rate limiter was called and connection counts were properly incremented
    pub async fn request<P, R>(
        self,
        method: &str,
        params: &P,
        revert_handler: RequestRevertHandler,
        unlocked_provider: Option<Arc<Web3Provider>>,
    ) -> Result<R, ProviderError>
    where
        // TODO: not sure about this type. would be better to not need clones, but measure and spawns combine to need it
        P: Clone + fmt::Debug + serde::Serialize + Send + Sync + 'static,
        R: serde::Serialize + serde::de::DeserializeOwned + fmt::Debug,
    {
        // TODO: use tracing spans
        // TODO: including params in this log is way too verbose
        // trace!(rpc=%self.conn, %method, "request");
        trace!("requesting from {}", self.conn);

        let mut provider: Option<Arc<Web3Provider>> = None;
        let mut logged = false;
        while provider.is_none() {
            // trace!("waiting on provider: locking...");

            // TODO: this should *not* be new_head_client. that is dedicated to only new heads
            if let Some(unlocked_provider) = unlocked_provider {
                provider = Some(unlocked_provider);
                break;
            }

            let unlocked_provider = self.conn.new_head_client.read().await;

            if let Some(unlocked_provider) = unlocked_provider.clone() {
                provider = Some(unlocked_provider);
                break;
            }

            if !logged {
                debug!("no provider for open handle on {}", self.conn);
                logged = true;
            }

            sleep(Duration::from_millis(100)).await;
        }

        let provider = provider.expect("provider was checked already");

        // TODO: replace ethers-rs providers with our own that supports streaming the responses
        let response = match provider.as_ref() {
            #[cfg(test)]
            Web3Provider::Mock => unimplemented!(),
            Web3Provider::Ws(p) => p.request(method, params).await,
            Web3Provider::Http(p) | Web3Provider::Both(p, _) => {
                // TODO: i keep hearing that http is faster. but ws has always been better for me. investigate more with actual benchmarks
                p.request(method, params).await
            }
        };

        // // TODO: i think ethers already has trace logging (and does it much more fancy)
        // trace!(
        //     "response from {} for {} {:?}: {:?}",
        //     self.conn,
        //     method,
        //     params,
        //     response,
        // );

        if let Err(err) = &response {
            // only save reverts for some types of calls
            // TODO: do something special for eth_sendRawTransaction too
            let revert_handler = if let RequestRevertHandler::Save = revert_handler {
                // TODO: should all these be Trace or Debug or a mix?
                if !["eth_call", "eth_estimateGas"].contains(&method) {
                    // trace!(%method, "skipping save on revert");
                    RequestRevertHandler::TraceLevel
                } else if self.authorization.db_conn.is_some() {
                    let log_revert_chance = self.authorization.checks.log_revert_chance;

                    if log_revert_chance == 0.0 {
                        // trace!(%method, "no chance. skipping save on revert");
                        RequestRevertHandler::TraceLevel
                    } else if log_revert_chance == 1.0 {
                        // trace!(%method, "gaurenteed chance. SAVING on revert");
                        revert_handler
                    } else if thread_fast_rng::thread_fast_rng().gen_range(0.0f64..=1.0)
                        < log_revert_chance
                    {
                        // trace!(%method, "missed chance. skipping save on revert");
                        RequestRevertHandler::TraceLevel
                    } else {
                        // trace!("Saving on revert");
                        // TODO: is always logging at debug level fine?
                        revert_handler
                    }
                } else {
                    // trace!(%method, "no database. skipping save on revert");
                    RequestRevertHandler::TraceLevel
                }
            } else {
                revert_handler
            };

            enum ResponseTypes {
                Revert,
                RateLimit,
                Ok,
            }

            // check for "execution reverted" here
            let response_type = if let ProviderError::JsonRpcClientError(err) = err {
                // Http and Ws errors are very similar, but different types
                let msg = match &*provider {
                    #[cfg(test)]
                    Web3Provider::Mock => unimplemented!(),
                    Web3Provider::Both(_, _) => {
                        if let Some(HttpClientError::JsonRpcError(err)) =
                            err.downcast_ref::<HttpClientError>()
                        {
                            Some(&err.message)
                        } else if let Some(WsClientError::JsonRpcError(err)) =
                            err.downcast_ref::<WsClientError>()
                        {
                            Some(&err.message)
                        } else {
                            None
                        }
                    }
                    Web3Provider::Http(_) => {
                        if let Some(HttpClientError::JsonRpcError(err)) =
                            err.downcast_ref::<HttpClientError>()
                        {
                            Some(&err.message)
                        } else {
                            None
                        }
                    }
                    Web3Provider::Ws(_) => {
                        if let Some(WsClientError::JsonRpcError(err)) =
                            err.downcast_ref::<WsClientError>()
                        {
                            Some(&err.message)
                        } else {
                            None
                        }
                    }
                };

                if let Some(msg) = msg {
                    if msg.starts_with("execution reverted") {
                        trace!("revert from {}", self.conn);
                        ResponseTypes::Revert
                    } else if msg.contains("limit") || msg.contains("request") {
                        trace!("rate limit from {}", self.conn);
                        ResponseTypes::RateLimit
                    } else {
                        ResponseTypes::Ok
                    }
                } else {
                    ResponseTypes::Ok
                }
            } else {
                ResponseTypes::Ok
            };

            if matches!(response_type, ResponseTypes::RateLimit) {
                if let Some(hard_limit_until) = self.conn.hard_limit_until.as_ref() {
                    let retry_at = Instant::now() + Duration::from_secs(1);

                    trace!("retry {} at: {:?}", self.conn, retry_at);

                    hard_limit_until.send_replace(retry_at);
                }
            }

            // TODO: think more about the method and param logs. those can be sensitive information
            match revert_handler {
                RequestRevertHandler::DebugLevel => {
                    // TODO: think about this revert check more. sometimes we might want reverts logged so this needs a flag
                    if matches!(response_type, ResponseTypes::Revert) {
                        debug!(
                            "bad response from {}! method={} params={:?} err={:?}",
                            self.conn, method, params, err
                        );
                    }
                }
                RequestRevertHandler::TraceLevel => {
                    trace!(
                        "bad response from {}! method={} params={:?} err={:?}",
                        self.conn,
                        method,
                        params,
                        err
                    );
                }
                RequestRevertHandler::ErrorLevel => {
                    // TODO: include params if not running in release mode
                    error!(
                        "bad response from {}! method={} err={:?}",
                        self.conn, method, err
                    );
                }
                RequestRevertHandler::WarnLevel => {
                    // TODO: include params if not running in release mode
                    warn!(
                        "bad response from {}! method={} err={:?}",
                        self.conn, method, err
                    );
                }
                RequestRevertHandler::Save => {
                    trace!(
                        "bad response from {}! method={} params={:?} err={:?}",
                        self.conn,
                        method,
                        params,
                        err
                    );

                    // TODO: do not unwrap! (doesn't matter much since we check method as a string above)
                    let method: Method = Method::try_from_value(&method.to_string()).unwrap();

                    // TODO: DO NOT UNWRAP! But also figure out the best way to keep returning ProviderErrors here
                    let params: EthCallParams = serde_json::from_value(json!(params))
                        .context("parsing params to EthCallParams")
                        .unwrap();

                    // spawn saving to the database so we don't slow down the request
                    let f = self.authorization.clone().save_revert(method, params.0 .0);

                    tokio::spawn(f);
                }
            }
        }

        response
    }
}
