/// this should move into web3_proxy once the basics are working
mod errors;
mod http;
mod http_proxy;
mod users;
mod ws_proxy;

use axum::{
    handler::Handler,
    response::IntoResponse,
    routing::{get, post},
    Extension, Router,
};
use entities::user_keys;
use reqwest::StatusCode;
use sea_orm::{
    ColumnTrait, DeriveColumn, EntityTrait, EnumIter, IdenStatic, QueryFilter, QuerySelect,
};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tracing::info;
use uuid::Uuid;

use crate::app::Web3ProxyApp;

use self::errors::handle_anyhow_error;

pub async fn rate_limit_by_ip(app: &Web3ProxyApp, ip: &IpAddr) -> Result<(), impl IntoResponse> {
    let rate_limiter_key = format!("ip:{}", ip);

    // TODO: dry this up with rate_limit_by_key
    if let Some(rate_limiter) = app.rate_limiter() {
        if rate_limiter
            .throttle_key(&rate_limiter_key, None, None, None)
            .await
            .is_err()
        {
            // TODO: set headers so they know when they can retry
            // warn!(?ip, "public rate limit exceeded");  // this is too verbose, but a stat might be good
            // TODO: use their id if possible
            return Err(handle_anyhow_error(
                Some(StatusCode::TOO_MANY_REQUESTS),
                None,
                anyhow::anyhow!(format!("too many requests from this ip: {}", ip)),
            )
            .await
            .into_response());
        }
    } else {
        // TODO: if no redis, rate limit with a local cache?
    }

    Ok(())
}

/// if Ok(()), rate limits are acceptable
/// if Err(response), rate limits exceeded
pub async fn rate_limit_by_key(
    app: &Web3ProxyApp,
    user_key: Uuid,
) -> Result<(), impl IntoResponse> {
    let db = app.db_conn();

    #[derive(Copy, Clone, Debug, EnumIter, DeriveColumn)]
    enum QueryAs {
        UserId,
    }

    // query the db to make sure this key is active
    // TODO: probably want a cache on this
    match user_keys::Entity::find()
        .select_only()
        .column_as(user_keys::Column::UserId, QueryAs::UserId)
        .filter(user_keys::Column::ApiKey.eq(user_key))
        .filter(user_keys::Column::Active.eq(true))
        .into_values::<_, QueryAs>()
        .one(db)
        .await
    {
        Ok::<Option<i64>, _>(Some(_)) => {
            // user key is valid
            if let Some(rate_limiter) = app.rate_limiter() {
                // TODO: check the db for this? maybe add to the find above with a join?
                let user_count_per_period = 100_000;
                // TODO: how does max burst actually work? what should it be?
                let user_max_burst = user_count_per_period;
                let user_period = 1;

                if rate_limiter
                    .throttle_key(
                        &user_key.to_string(),
                        Some(user_max_burst),
                        Some(user_count_per_period),
                        Some(user_period),
                    )
                    .await
                    .is_err()
                {
                    // TODO: set headers so they know when they can retry
                    // warn!(?ip, "public rate limit exceeded");  // this is too verbose, but a stat might be good
                    // TODO: use their id if possible
                    return Err(handle_anyhow_error(
                        Some(StatusCode::TOO_MANY_REQUESTS),
                        None,
                        // TODO: include the user id (NOT THE API KEY!) here
                        anyhow::anyhow!("too many requests from this key"),
                    )
                    .await
                    .into_response());
                }
            } else {
                // TODO: if no redis, rate limit with a local cache?
            }
        }
        Ok(None) => {
            // invalid user key
            // TODO: rate limit by ip here, too? maybe tarpit?
            return Err(handle_anyhow_error(
                Some(StatusCode::FORBIDDEN),
                None,
                anyhow::anyhow!("unknown api key"),
            )
            .await
            .into_response());
        }
        Err(err) => {
            let err: anyhow::Error = err.into();

            return Err(handle_anyhow_error(
                Some(StatusCode::INTERNAL_SERVER_ERROR),
                None,
                err.context("failed checking database for user key"),
            )
            .await
            .into_response());
        }
    }

    Ok(())
}

pub async fn run(port: u16, proxy_app: Arc<Web3ProxyApp>) -> anyhow::Result<()> {
    // build our application with a route
    // order most to least common
    let app = Router::new()
        .route("/", post(http_proxy::public_proxy_web3_rpc))
        .route("/", get(ws_proxy::public_websocket_handler))
        .route("/u/:user_key", post(http_proxy::user_proxy_web3_rpc))
        .route("/u/:user_key", get(ws_proxy::user_websocket_handler))
        .route("/health", get(http::health))
        .route("/status", get(http::status))
        .route("/users", post(users::create_user))
        .layer(Extension(proxy_app));

    // 404 for any unknown routes
    let app = app.fallback(errors::handler_404.into_service());

    // run our app with hyper
    // `axum::Server` is a re-export of `hyper::Server`
    // TODO: allow only listening on localhost?
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!("listening on port {}", port);
    // TODO: into_make_service is enough if we always run behind a proxy. make into_make_service_with_connect_info optional?
    axum::Server::bind(&addr)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .map_err(Into::into)
}
