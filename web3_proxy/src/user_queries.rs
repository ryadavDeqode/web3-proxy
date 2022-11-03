use anyhow::Context;
use axum::{
    headers::{authorization::Bearer, Authorization},
    TypedHeader,
};
use chrono::NaiveDateTime;
use entities::{rpc_accounting, rpc_key};
use hashbrown::HashMap;
use migration::{Expr, SimpleExpr};
use num::Zero;
use redis_rate_limiter::{redis::AsyncCommands, RedisConnection};
use sea_orm::{
    ColumnTrait, Condition, EntityTrait, JoinType, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect, RelationTrait,
};
use tracing::{instrument, warn};

use crate::{app::Web3ProxyApp, user_token::UserBearerToken};

/// get the attached address from redis for the given auth_token.
/// 0 means all users
#[instrument(level = "trace", skip(redis_conn))]
async fn get_user_id_from_params(
    mut redis_conn: RedisConnection,
    // this is a long type. should we strip it down?
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    params: &HashMap<String, String>,
) -> anyhow::Result<u64> {
    match (bearer, params.get("user_id")) {
        (Some(TypedHeader(Authorization(bearer))), Some(user_id)) => {
            // check for the bearer cache key
            let bearer_cache_key = UserBearerToken::try_from(bearer)?.to_string();

            // get the user id that is attached to this bearer token
            let bearer_user_id = redis_conn
                .get::<_, u64>(bearer_cache_key)
                .await
                // TODO: this should be a 403
                .context("fetching rpc_key_id from redis with bearer_cache_key")?;

            let user_id: u64 = user_id.parse().context("Parsing user_id param")?;

            if bearer_user_id != user_id {
                // TODO: proper HTTP Status code
                Err(anyhow::anyhow!("permission denied"))
            } else {
                Ok(bearer_user_id)
            }
        }
        (_, None) => {
            // they have a bearer token. we don't care about it on public pages
            // 0 means all
            Ok(0)
        }
        (None, Some(x)) => {
            // they do not have a bearer token, but requested a specific id. block
            // TODO: proper error code
            // TODO: maybe instead of this sharp edged warn, we have a config value?
            // TODO: check config for if we should deny or allow this
            // Err(anyhow::anyhow!("permission denied"))

            // TODO: make this a flag
            warn!("allowing without auth during development!");
            Ok(x.parse()?)
        }
    }
}

/// only allow rpc_key to be set if user_id is also set.
/// this will keep people from reading someone else's keys.
/// 0 means none.
#[instrument(level = "trace")]
pub fn get_rpc_key_id_from_params(
    user_id: u64,
    params: &HashMap<String, String>,
) -> anyhow::Result<u64> {
    if user_id > 0 {
        params.get("rpc_key_id").map_or_else(
            || Ok(0),
            |c| {
                let c = c.parse()?;

                Ok(c)
            },
        )
    } else {
        Ok(0)
    }
}

#[instrument(level = "trace")]
pub fn get_chain_id_from_params(
    app: &Web3ProxyApp,
    params: &HashMap<String, String>,
) -> anyhow::Result<u64> {
    params.get("chain_id").map_or_else(
        || Ok(app.config.chain_id),
        |c| {
            let c = c.parse()?;

            Ok(c)
        },
    )
}

#[instrument(level = "trace")]
pub fn get_query_start_from_params(
    params: &HashMap<String, String>,
) -> anyhow::Result<chrono::NaiveDateTime> {
    params.get("query_start").map_or_else(
        || {
            // no timestamp in params. set default
            let x = chrono::Utc::now() - chrono::Duration::days(30);

            Ok(x.naive_utc())
        },
        |x: &String| {
            // parse the given timestamp
            let x = x.parse::<i64>().context("parsing timestamp query param")?;

            // TODO: error code 401
            let x =
                NaiveDateTime::from_timestamp_opt(x, 0).context("parsing timestamp query param")?;

            Ok(x)
        },
    )
}

#[instrument(level = "trace")]
pub fn get_page_from_params(params: &HashMap<String, String>) -> anyhow::Result<u64> {
    params.get("page").map_or_else::<anyhow::Result<u64>, _, _>(
        || {
            // no page in params. set default
            Ok(0)
        },
        |x: &String| {
            // parse the given timestamp
            // TODO: error code 401
            let x = x.parse().context("parsing page query from params")?;

            Ok(x)
        },
    )
}

#[instrument(level = "trace")]
pub fn get_query_window_seconds_from_params(
    params: &HashMap<String, String>,
) -> anyhow::Result<u64> {
    params.get("query_window_seconds").map_or_else(
        || {
            // no page in params. set default
            Ok(0)
        },
        |x: &String| {
            // parse the given timestamp
            // TODO: error code 401
            let x = x
                .parse()
                .context("parsing query window seconds from params")?;

            Ok(x)
        },
    )
}

/// stats aggregated across a time period
/// TODO: aggregate on everything, or let the caller decide?
#[instrument(level = "trace")]
pub async fn get_aggregate_rpc_stats_from_params(
    app: &Web3ProxyApp,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    params: HashMap<String, String>,
) -> anyhow::Result<HashMap<&str, serde_json::Value>> {
    let db_conn = app.db_conn().context("connecting to db")?;
    let redis_conn = app.redis_conn().await.context("connecting to redis")?;

    let mut response = HashMap::new();

    let page = get_page_from_params(&params)?;
    response.insert("page", serde_json::to_value(page)?);

    // TODO: page size from param with a max from the config
    let page_size = 200;
    response.insert("page_size", serde_json::to_value(page_size)?);

    let q = rpc_accounting::Entity::find()
        .select_only()
        .column_as(
            rpc_accounting::Column::FrontendRequests.sum(),
            "total_requests",
        )
        .column_as(
            rpc_accounting::Column::BackendRequests.sum(),
            "total_backend_retries",
        )
        .column_as(
            rpc_accounting::Column::CacheMisses.sum(),
            "total_cache_misses",
        )
        .column_as(rpc_accounting::Column::CacheHits.sum(), "total_cache_hits")
        .column_as(
            rpc_accounting::Column::SumResponseBytes.sum(),
            "total_response_bytes",
        )
        .column_as(
            // TODO: can we sum bools like this?
            rpc_accounting::Column::ErrorResponse.sum(),
            "total_error_responses",
        )
        .column_as(
            rpc_accounting::Column::SumResponseMillis.sum(),
            "total_response_millis",
        );

    let condition = Condition::all();

    // TODO: DRYer! move this onto query_window_seconds_from_params?
    let query_window_seconds = get_query_window_seconds_from_params(&params)?;
    let q = if query_window_seconds.is_zero() {
        // TODO: order by more than this?
        // query_window_seconds is not set so we aggregate all records
        // TODO: i am pretty sure we need to filter by something
        q
    } else {
        // TODO: is there a better way to do this? how can we get "period_datetime" into this with types?
        // TODO: how can we get the first window to start at query_start_timestamp
        let expr = Expr::cust_with_values(
            "FLOOR(UNIX_TIMESTAMP(rpc_accounting.period_datetime) / ?) * ?",
            [query_window_seconds, query_window_seconds],
        );

        response.insert(
            "query_window_seconds",
            serde_json::to_value(query_window_seconds)?,
        );

        q.column_as(expr, "query_window")
            .group_by(Expr::cust("query_window"))
            // TODO: is there a simpler way to order_by?
            .order_by_asc(SimpleExpr::Custom("query_window".to_string()))
    };

    // aggregate stats after query_start
    // TODO: minimum query_start of 90 days?
    let query_start = get_query_start_from_params(&params)?;
    // TODO: if no query_start, don't add to response or condition
    response.insert(
        "query_start",
        serde_json::to_value(query_start.timestamp() as u64)?,
    );
    let condition = condition.add(rpc_accounting::Column::PeriodDatetime.gte(query_start));

    // filter on chain_id
    let chain_id = get_chain_id_from_params(app, &params)?;
    let (condition, q) = if chain_id.is_zero() {
        // fetch all the chains. don't filter or aggregate
        (condition, q)
    } else {
        let condition = condition.add(rpc_accounting::Column::ChainId.eq(chain_id));

        response.insert("chain_id", serde_json::to_value(chain_id)?);

        (condition, q)
    };

    // filter on user_id
    // TODO: what about filter on rpc_key_id?
    // get_user_id_from_params checks that the bearer is connected to this user_id
    let user_id = get_user_id_from_params(redis_conn, bearer, &params).await?;
    let (condition, q) = if user_id.is_zero() {
        // 0 means everyone. don't filter on user
        (condition, q)
    } else {
        // TODO: are these joins correct? do we need these columns?
        // TODO: also join on on keys where user is a secondary user?
        let q = q
            .join(JoinType::InnerJoin, rpc_accounting::Relation::RpcKey.def())
            .column(rpc_accounting::Column::Id)
            .column(rpc_key::Column::Id)
            .join(JoinType::InnerJoin, rpc_key::Relation::User.def())
            .column(rpc_key::Column::UserId);

        let condition = condition.add(rpc_key::Column::UserId.eq(user_id));

        (condition, q)
    };

    // now that all the conditions are set up. add them to the query
    let q = q.filter(condition);

    // TODO: trace log query here? i think sea orm has a useful log level for this

    // query the database
    let aggregate = q
        .into_json()
        .paginate(&db_conn, page_size)
        .fetch_page(page)
        .await?;

    // add the query response to the response
    response.insert("aggregate", serde_json::Value::Array(aggregate));

    Ok(response)
}

/// stats grouped by key_id and error_repsponse and method and key
#[instrument(level = "trace")]
pub async fn get_detailed_stats(
    app: &Web3ProxyApp,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    params: HashMap<String, String>,
) -> anyhow::Result<HashMap<&str, serde_json::Value>> {
    let db_conn = app.db_conn().context("connecting to db")?;
    let redis_conn = app.redis_conn().await.context("connecting to redis")?;

    let user_id = get_user_id_from_params(redis_conn, bearer, &params).await?;
    let rpc_key_id = get_rpc_key_id_from_params(user_id, &params)?;
    let chain_id = get_chain_id_from_params(app, &params)?;
    let query_start = get_query_start_from_params(&params)?;
    let query_window_seconds = get_query_window_seconds_from_params(&params)?;
    let page = get_page_from_params(&params)?;
    // TODO: handle secondary users, too

    // TODO: page size from config? from params with a max in the config?
    let page_size = 200;

    // TODO: minimum query_start of 90 days?

    let mut response = HashMap::new();

    response.insert("page", serde_json::to_value(page)?);
    response.insert("page_size", serde_json::to_value(page_size)?);
    response.insert("chain_id", serde_json::to_value(chain_id)?);
    response.insert(
        "query_start",
        serde_json::to_value(query_start.timestamp() as u64)?,
    );

    // TODO: how do we get count reverts compared to other errors? does it matter? what about http errors to our users?
    // TODO: how do we count uptime?
    let q = rpc_accounting::Entity::find()
        .select_only()
        // groups
        .column(rpc_accounting::Column::ErrorResponse)
        .group_by(rpc_accounting::Column::ErrorResponse)
        .column(rpc_accounting::Column::Method)
        .group_by(rpc_accounting::Column::Method)
        .column(rpc_accounting::Column::ArchiveRequest)
        .group_by(rpc_accounting::Column::ArchiveRequest)
        // chain id is added later
        // aggregate columns
        .column_as(
            rpc_accounting::Column::FrontendRequests.sum(),
            "total_requests",
        )
        .column_as(
            rpc_accounting::Column::BackendRequests.sum(),
            "total_backend_requests",
        )
        .column_as(
            rpc_accounting::Column::CacheMisses.sum(),
            "total_cache_misses",
        )
        .column_as(rpc_accounting::Column::CacheHits.sum(), "total_cache_hits")
        .column_as(
            rpc_accounting::Column::SumResponseBytes.sum(),
            "total_response_bytes",
        )
        .column_as(
            // TODO: can we sum bools like this?
            rpc_accounting::Column::ErrorResponse.sum(),
            "total_error_responses",
        )
        .column_as(
            rpc_accounting::Column::SumResponseMillis.sum(),
            "total_response_millis",
        )
        // TODO: order on method next?
        .order_by_asc(rpc_accounting::Column::PeriodDatetime.min());

    let condition = Condition::all().add(rpc_accounting::Column::PeriodDatetime.gte(query_start));

    let (condition, q) = if chain_id.is_zero() {
        // fetch all the chains. don't filter
        // TODO: wait. do we want chain id on the logs? we can get that by joining key
        let q = q
            .column(rpc_accounting::Column::ChainId)
            .group_by(rpc_accounting::Column::ChainId);

        (condition, q)
    } else {
        let condition = condition.add(rpc_accounting::Column::ChainId.eq(chain_id));

        (condition, q)
    };

    let (condition, q) = if user_id != 0 || rpc_key_id != 0 {
        // if user id or rpc key id is specified, we need to join on at least rpc_key_id
        let q = q
            .join(JoinType::InnerJoin, rpc_accounting::Relation::RpcKey.def())
            .column(rpc_key::Column::Id);

        // .group_by(rpc_key::Column::Id);

        let condition = condition.add(rpc_key::Column::UserId.eq(user_id));

        (condition, q)
    } else {
        // both user_id and rpc_key_id are 0, show aggregate stats
        (condition, q)
    };

    let (condition, q) = if user_id == 0 {
        // 0 means everyone. don't filter on user_key_id
        (condition, q)
    } else {
        // TODO: add authentication here! make sure this user_id is owned by the authenticated user
        // TODO: what about keys where this user is a secondary user?
        let q = q
            .join(JoinType::InnerJoin, rpc_accounting::Relation::RpcKey.def())
            .column(rpc_key::Column::Id)
            .group_by(rpc_key::Column::Id);

        let condition = condition.add(rpc_key::Column::UserId.eq(user_id));

        let q = if rpc_key_id == 0 {
            q.column(rpc_key::Column::UserId)
                .group_by(rpc_key::Column::UserId)
        } else {
            response.insert("rpc_key_id", serde_json::to_value(rpc_key_id)?);

            // no need to group_by user_id when we are grouping by key_id
            q.column(rpc_key::Column::Id).group_by(rpc_key::Column::Id)
        };

        (condition, q)
    };

    let q = if query_window_seconds != 0 {
        /*
        let query_start_timestamp: u64 = query_start
            .timestamp()
            .try_into()
            .context("query_start to timestamp")?;
        */
        // TODO: is there a better way to do this? how can we get "period_datetime" into this with types?
        // TODO: how can we get the first window to start at query_start_timestamp
        let expr = Expr::cust_with_values(
            "FLOOR(UNIX_TIMESTAMP(rpc_accounting.period_datetime) / ?) * ?",
            [query_window_seconds, query_window_seconds],
        );

        response.insert(
            "query_window_seconds",
            serde_json::to_value(query_window_seconds)?,
        );

        q.column_as(expr, "query_window_seconds")
            .group_by(Expr::cust("query_window_seconds"))
    } else {
        // TODO: order by more than this?
        // query_window_seconds is not set so we aggregate all records
        q
    };

    let q = q.filter(condition);

    // log query here. i think sea orm has a useful log level for this

    // TODO: transform this into a nested hashmap instead of a giant table?
    let r = q
        .into_json()
        .paginate(&db_conn, page_size)
        .fetch_page(page)
        .await?;

    response.insert("detailed_aggregate", serde_json::Value::Array(r));

    // number of keys
    // number of secondary keys
    // avg and max concurrent requests per second per api key

    Ok(response)
}
