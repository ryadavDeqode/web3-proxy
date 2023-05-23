//! Handle registration, logins, and managing account data.
use crate::app::Web3ProxyApp;
use crate::frontend::authorization::{login_is_authorized, RpcSecretKey};
use crate::frontend::errors::{Web3ProxyError, Web3ProxyErrorContext, Web3ProxyResponse};
use crate::user_token::UserBearerToken;
use crate::{PostLogin, PostLoginQuery};
use axum::{
    extract::{Path, Query},
    headers::{authorization::Bearer, Authorization},
    response::IntoResponse,
    Extension, Json, TypedHeader,
};
use axum_client_ip::InsecureClientIp;
use axum_macros::debug_handler;
use chrono::{TimeZone, Utc};
use entities;
use entities::{balance, login, pending_login, referee, referrer, rpc_key, user};
use ethers::{prelude::Address, types::Bytes};
use hashbrown::HashMap;
use http::StatusCode;
use log::{debug, warn};
use migration::sea_orm::prelude::{Decimal, Uuid};
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter,
    TransactionTrait,
};
use serde_json::json;
use siwe::{Message, VerificationOpts};
use std::ops::Add;
use std::str::FromStr;
use std::sync::Arc;
use time::{Duration, OffsetDateTime};
use ulid::Ulid;

/// `GET /user/login/:user_address` or `GET /user/login/:user_address/:message_eip` -- Start the "Sign In with Ethereum" (siwe) login flow.
///
/// `message_eip`s accepted:
///   - eip191_bytes
///   - eip191_hash
///   - eip4361 (default)
///
/// Coming soon: eip1271
///
/// This is the initial entrypoint for logging in. Take the response from this endpoint and give it to your user's wallet for singing. POST the response to `/user/login`.
///
/// Rate limited by IP address.
///
/// At first i thought about checking that user_address is in our db,
/// But theres no need to separate the registration and login flows.
/// It is a better UX to just click "login with ethereum" and have the account created if it doesn't exist.
/// We can prompt for an email and and payment after they log in.
#[debug_handler]
pub async fn user_login_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    InsecureClientIp(ip): InsecureClientIp,
    // TODO: what does axum's error handling look like if the path fails to parse?
    Path(mut params): Path<HashMap<String, String>>,
) -> Web3ProxyResponse {
    login_is_authorized(&app, ip).await?;

    // create a message and save it in redis
    // TODO: how many seconds? get from config?
    let expire_seconds: usize = 20 * 60;

    let nonce = Ulid::new();

    let issued_at = OffsetDateTime::now_utc();

    let expiration_time = issued_at.add(Duration::new(expire_seconds as i64, 0));

    // TODO: allow ENS names here?
    let user_address: Address = params
        .remove("user_address")
        .ok_or(Web3ProxyError::BadRouting)?
        .parse()
        .or(Err(Web3ProxyError::ParseAddressError))?;

    let login_domain = app
        .config
        .login_domain
        .clone()
        .unwrap_or_else(|| "llamanodes.com".to_string());

    // TODO: get most of these from the app config
    let message = Message {
        // TODO: don't unwrap
        // TODO: accept a login_domain from the request?
        domain: login_domain.parse().unwrap(),
        address: user_address.to_fixed_bytes(),
        // TODO: config for statement
        statement: Some("🦙🦙🦙🦙🦙".to_string()),
        // TODO: don't unwrap
        uri: format!("https://{}/", login_domain).parse().unwrap(),
        version: siwe::Version::V1,
        chain_id: 1,
        expiration_time: Some(expiration_time.into()),
        issued_at: issued_at.into(),
        nonce: nonce.to_string(),
        not_before: None,
        request_id: None,
        resources: vec![],
    };

    let db_conn = app.db_conn().web3_context("login requires a database")?;

    // massage types to fit in the database. sea-orm does not make this very elegant
    let uuid = Uuid::from_u128(nonce.into());
    // we add 1 to expire_seconds just to be sure the database has the key for the full expiration_time
    let expires_at = Utc
        .timestamp_opt(expiration_time.unix_timestamp() + 1, 0)
        .unwrap();

    // we do not store a maximum number of attempted logins. anyone can request so we don't want to allow DOS attacks
    // add a row to the database for this user
    let user_pending_login = pending_login::ActiveModel {
        id: sea_orm::NotSet,
        nonce: sea_orm::Set(uuid),
        message: sea_orm::Set(message.to_string()),
        expires_at: sea_orm::Set(expires_at),
        imitating_user: sea_orm::Set(None),
    };

    user_pending_login
        .save(&db_conn)
        .await
        .web3_context("saving user's pending_login")?;

    // there are multiple ways to sign messages and not all wallets support them
    // TODO: default message eip from config?
    let message_eip = params
        .remove("message_eip")
        .unwrap_or_else(|| "eip4361".to_string());

    let message: String = match message_eip.as_str() {
        "eip191_bytes" => Bytes::from(message.eip191_bytes().unwrap()).to_string(),
        "eip191_hash" => Bytes::from(&message.eip191_hash().unwrap()).to_string(),
        "eip4361" => message.to_string(),
        _ => {
            return Err(Web3ProxyError::InvalidEip);
        }
    };

    Ok(message.into_response())
}

/// `POST /user/login` - Register or login by posting a signed "siwe" message.
/// It is recommended to save the returned bearer token in a cookie.
/// The bearer token can be used to authenticate other requests, such as getting the user's stats or modifying the user's profile.
#[debug_handler]
pub async fn user_login_post(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    InsecureClientIp(ip): InsecureClientIp,
    Query(query): Query<PostLoginQuery>,
    Json(payload): Json<PostLogin>,
) -> Web3ProxyResponse {
    login_is_authorized(&app, ip).await?;

    // TODO: this seems too verbose. how can we simply convert a String into a [u8; 65]
    let their_sig_bytes = Bytes::from_str(&payload.sig).web3_context("parsing sig")?;
    if their_sig_bytes.len() != 65 {
        return Err(Web3ProxyError::InvalidSignatureLength);
    }
    let mut their_sig: [u8; 65] = [0; 65];
    for x in 0..65 {
        their_sig[x] = their_sig_bytes[x]
    }

    // we can't trust that they didn't tamper with the message in some way. like some clients return it hex encoded
    // TODO: checking 0x seems fragile, but I think it will be fine. siwe message text shouldn't ever start with 0x
    let their_msg: Message = if payload.msg.starts_with("0x") {
        let their_msg_bytes =
            Bytes::from_str(&payload.msg).web3_context("parsing payload message")?;

        // TODO: lossy or no?
        String::from_utf8_lossy(their_msg_bytes.as_ref())
            .parse::<siwe::Message>()
            .web3_context("parsing hex string message")?
    } else {
        payload
            .msg
            .parse::<siwe::Message>()
            .web3_context("parsing string message")?
    };

    // the only part of the message we will trust is their nonce
    // TODO: this is fragile. have a helper function/struct for redis keys
    let login_nonce = UserBearerToken::from_str(&their_msg.nonce)?;

    // fetch the message we gave them from our database
    let db_replica = app
        .db_replica()
        .web3_context("Getting database connection")?;

    // massage type for the db
    let login_nonce_uuid: Uuid = login_nonce.clone().into();

    let user_pending_login = pending_login::Entity::find()
        .filter(pending_login::Column::Nonce.eq(login_nonce_uuid))
        .one(db_replica.conn())
        .await
        .web3_context("database error while finding pending_login")?
        .web3_context("login nonce not found")?;

    let our_msg: siwe::Message = user_pending_login
        .message
        .parse()
        .web3_context("parsing siwe message")?;

    // default options are fine. the message includes timestamp and domain and nonce
    let verify_config = VerificationOpts::default();

    // Check with both verify and verify_eip191
    if let Err(err_1) = our_msg
        .verify(&their_sig, &verify_config)
        .await
        .web3_context("verifying signature against our local message")
    {
        // verification method 1 failed. try eip191
        if let Err(err_191) = our_msg
            .verify_eip191(&their_sig)
            .web3_context("verifying eip191 signature against our local message")
        {
            let db_conn = app
                .db_conn()
                .web3_context("deleting expired pending logins requires a db")?;

            // delete ALL expired rows.
            let now = Utc::now();
            let delete_result = pending_login::Entity::delete_many()
                .filter(pending_login::Column::ExpiresAt.lte(now))
                .exec(&db_conn)
                .await?;

            // TODO: emit a stat? if this is high something weird might be happening
            debug!("cleared expired pending_logins: {:?}", delete_result);

            return Err(Web3ProxyError::EipVerificationFailed(
                Box::new(err_1),
                Box::new(err_191),
            ));
        }
    }

    // TODO: limit columns or load whole user?
    let caller = user::Entity::find()
        .filter(user::Column::Address.eq(our_msg.address.as_ref()))
        .one(db_replica.conn())
        .await?;

    let db_conn = app.db_conn().web3_context("login requires a db")?;

    let (caller, user_rpc_keys, status_code) = match caller {
        None => {
            // user does not exist yet

            // check the invite code
            // TODO: more advanced invite codes that set different request/minute and concurrency limits
            // Do nothing if app config is none (then there is basically no authentication invitation, and the user can process with a free tier ...

            // Prematurely return if there is a wrong invite code
            if let Some(invite_code) = &app.config.invite_code {
                if query.invite_code.as_ref() != Some(invite_code) {
                    return Err(Web3ProxyError::InvalidInviteCode);
                }
            }

            let txn = db_conn.begin().await?;

            // First add a user

            // the only thing we need from them is an address
            // everything else is optional
            // TODO: different invite codes should allow different levels
            // TODO: maybe decrement a count on the invite code?
            // TODO: There will be two different transactions. The first one inserts the user, the second one marks the user as being referred
            let caller = user::ActiveModel {
                address: sea_orm::Set(our_msg.address.into()),
                ..Default::default()
            };

            let caller = caller.insert(&txn).await?;

            // create the user's first api key
            let rpc_secret_key = RpcSecretKey::new();

            let user_rpc_key = rpc_key::ActiveModel {
                user_id: sea_orm::Set(caller.id),
                secret_key: sea_orm::Set(rpc_secret_key.into()),
                description: sea_orm::Set(None),
                ..Default::default()
            };

            let user_rpc_key = user_rpc_key
                .insert(&txn)
                .await
                .web3_context("Failed saving new user key")?;

            // We should also create the balance entry ...
            let user_balance = balance::ActiveModel {
                user_id: sea_orm::Set(caller.id),
                available_balance: sea_orm::Set(Decimal::new(0, 0)),
                used_balance: sea_orm::Set(Decimal::new(0, 0)),
                ..Default::default()
            };
            user_balance.insert(&txn).await?;

            let user_rpc_keys = vec![user_rpc_key];

            // Also add a part for the invite code, i.e. who invited this guy

            // save the user and key to the database
            txn.commit().await?;

            let txn = db_conn.begin().await?;
            // First, optionally catch a referral code from the parameters if there is any
            debug!("Refferal code is: {:?}", payload.referral_code);
            if let Some(referral_code) = payload.referral_code.as_ref() {
                // If it is not inside, also check in the database
                warn!("Using register referral code:  {:?}", referral_code);
                let user_referrer = referrer::Entity::find()
                    .filter(referrer::Column::ReferralCode.eq(referral_code))
                    .one(db_replica.conn())
                    .await?
                    .ok_or(Web3ProxyError::UnknownReferralCode)?;

                // Create a new item in the database,
                // marking this guy as the referrer (and ignoring a duplicate insert, if there is any...)
                // First person to make the referral gets all credits
                // Generate a random referral code ...
                let used_referral = referee::ActiveModel {
                    used_referral_code: sea_orm::Set(user_referrer.id),
                    user_id: sea_orm::Set(caller.id),
                    credits_applied_for_referee: sea_orm::Set(false),
                    credits_applied_for_referrer: sea_orm::Set(Decimal::new(0, 10)),
                    ..Default::default()
                };
                used_referral.insert(&txn).await?;
            }
            txn.commit().await?;

            (caller, user_rpc_keys, StatusCode::CREATED)
        }
        Some(caller) => {
            // Let's say that a user that exists can actually also redeem a key in retrospect...
            let txn = db_conn.begin().await?;
            // TODO: Move this into a common variable outside ...
            // First, optionally catch a referral code from the parameters if there is any
            if let Some(referral_code) = payload.referral_code.as_ref() {
                // If it is not inside, also check in the database
                warn!("Using referral code: {:?}", referral_code);
                let user_referrer = referrer::Entity::find()
                    .filter(referrer::Column::ReferralCode.eq(referral_code))
                    .one(db_replica.conn())
                    .await?
                    .ok_or(Web3ProxyError::BadRequest(format!(
                        "The referral_link you provided does not exist {}",
                        referral_code
                    )))?;

                // Create a new item in the database,
                // marking this guy as the referrer (and ignoring a duplicate insert, if there is any...)
                // First person to make the referral gets all credits
                // Generate a random referral code ...
                let used_referral = referee::ActiveModel {
                    used_referral_code: sea_orm::Set(user_referrer.id),
                    user_id: sea_orm::Set(caller.id),
                    credits_applied_for_referee: sea_orm::Set(false),
                    credits_applied_for_referrer: sea_orm::Set(Decimal::new(0, 10)),
                    ..Default::default()
                };
                used_referral.insert(&txn).await?;
            }
            txn.commit().await?;

            // the user is already registered
            let user_rpc_keys = rpc_key::Entity::find()
                .filter(rpc_key::Column::UserId.eq(caller.id))
                .all(db_replica.conn())
                .await
                .web3_context("failed loading user's key")?;

            (caller, user_rpc_keys, StatusCode::OK)
        }
    };

    // create a bearer token for the user.
    let user_bearer_token = UserBearerToken::default();

    // json response with everything in it
    // we could return just the bearer token, but I think they will always request api keys and the user profile
    let response_json = json!({
        "rpc_keys": user_rpc_keys
            .into_iter()
            .map(|user_rpc_key| (user_rpc_key.id, user_rpc_key))
            .collect::<HashMap<_, _>>(),
        "bearer_token": user_bearer_token,
        "user": caller,
    });

    let response = (status_code, Json(response_json)).into_response();

    // add bearer to the database

    // expire in 4 weeks
    let expires_at = Utc::now()
        .checked_add_signed(chrono::Duration::weeks(4))
        .unwrap();

    let user_login = login::ActiveModel {
        id: sea_orm::NotSet,
        bearer_token: sea_orm::Set(user_bearer_token.uuid()),
        user_id: sea_orm::Set(caller.id),
        expires_at: sea_orm::Set(expires_at),
        read_only: sea_orm::Set(false),
    };

    user_login
        .save(&db_conn)
        .await
        .web3_context("saving user login")?;

    if let Err(err) = user_pending_login
        .into_active_model()
        .delete(&db_conn)
        .await
    {
        warn!("Failed to delete nonce:{}: {}", login_nonce.0, err);
    }

    Ok(response)
}

/// `POST /user/logout` - Forget the bearer token in the `Authentication` header.
#[debug_handler]
pub async fn user_logout_post(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
) -> Web3ProxyResponse {
    let user_bearer = UserBearerToken::try_from(bearer)?;

    let db_conn = app
        .db_conn()
        .web3_context("database needed for user logout")?;

    if let Err(err) = login::Entity::delete_many()
        .filter(login::Column::BearerToken.eq(user_bearer.uuid()))
        .exec(&db_conn)
        .await
    {
        debug!("Failed to delete {}: {}", user_bearer.redis_key(), err);
    }

    let now = Utc::now();

    // also delete any expired logins
    let delete_result = login::Entity::delete_many()
        .filter(login::Column::ExpiresAt.lte(now))
        .exec(&db_conn)
        .await;

    debug!("Deleted expired logins: {:?}", delete_result);

    // also delete any expired pending logins
    let delete_result = login::Entity::delete_many()
        .filter(login::Column::ExpiresAt.lte(now))
        .exec(&db_conn)
        .await;

    debug!("Deleted expired pending logins: {:?}", delete_result);

    // TODO: what should the response be? probably json something
    Ok("goodbye".into_response())
}