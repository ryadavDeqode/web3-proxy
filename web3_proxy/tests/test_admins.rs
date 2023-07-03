mod common;

use std::str::FromStr;
use std::time::Duration;

use crate::common::TestApp;
use ethers::prelude::Signer;
use ethers::types::Signature;
use rust_decimal::Decimal;
use tracing::info;
use web3_proxy::frontend::admin::AdminIncreaseBalancePost;
use web3_proxy::frontend::users::authentication::{LoginPostResponse, PostLogin};
use web3_proxy::sub_commands::ChangeAdminStatusSubCommand;

// #[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[ignore = "under construction"]
#[test_log::test(tokio::test)]
async fn test_admin_imitate_user() {
    let x = TestApp::spawn(true).await;

    todo!();
}

#[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[test_log::test(tokio::test)]
async fn test_admin_grant_credits() {
    info!("Starting admin grant credits test");
    let x = TestApp::spawn(true).await;
    let r = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();

    // Setup variables that will be used
    let login_post_url = format!("{}user/login", x.proxy_provider.url());
    let increase_balance_post_url = format!("{}admin/increase_balance", x.proxy_provider.url());

    let admin_wallet = x.wallet(1);
    let user_wallet = x.wallet(2);

    // Login the admin to create their account. they aren't an admin yet
    let admin_login_get_url = format!(
        "{}user/login/{:?}",
        x.proxy_provider.url(),
        admin_wallet.address()
    );
    let admin_login_message = r.get(admin_login_get_url).send().await.unwrap();
    let admin_login_message = admin_login_message.text().await.unwrap();

    // Sign the message and POST it to login as admin
    let admin_signed: Signature = admin_wallet
        .sign_message(&admin_login_message)
        .await
        .unwrap();
    info!(?admin_signed);

    let admin_post_login_data = PostLogin {
        msg: admin_login_message,
        sig: admin_signed.to_string(),
        referral_code: None,
    };
    info!(?admin_post_login_data);

    let admin_login_response = r
        .post(&login_post_url)
        .json(&admin_post_login_data)
        .send()
        .await
        .unwrap()
        .json::<LoginPostResponse>()
        .await
        .unwrap();
    info!(?admin_login_response);

    // Also login the user (to create the user)
    let user_login_get_url = format!(
        "{}user/login/{:?}",
        x.proxy_provider.url(),
        user_wallet.address()
    );
    let user_login_message = r.get(user_login_get_url).send().await.unwrap();
    let user_login_message = user_login_message.text().await.unwrap();

    // Sign the message and POST it to login as the user
    let user_signed: Signature = user_wallet.sign_message(&user_login_message).await.unwrap();
    info!(?user_signed);

    let user_post_login_data = PostLogin {
        msg: user_login_message,
        sig: user_signed.to_string(),
        referral_code: None,
    };
    info!(?user_post_login_data);

    let user_login_response = r
        .post(&login_post_url)
        .json(&user_post_login_data)
        .send()
        .await
        .unwrap()
        .json::<LoginPostResponse>()
        .await
        .unwrap();
    info!(?user_login_response);

    info!("Make the user an admin ...");
    // Change Admin SubCommand struct
    let admin_status_changer = ChangeAdminStatusSubCommand {
        address: format!("{:?}", admin_wallet.address()),
        should_be_admin: true,
    };
    info!(?admin_status_changer);

    info!("Changing the status of the admin_wallet to be an admin");
    // Pass on the database into it ...
    admin_status_changer.main(x.db_conn()).await.unwrap();

    // Login the admin again, because he was just signed out
    let admin_login_get_url = format!(
        "{}user/login/{:?}",
        x.proxy_provider.url(),
        admin_wallet.address()
    );
    let admin_login_message = r.get(admin_login_get_url).send().await.unwrap();
    let admin_login_message = admin_login_message.text().await.unwrap();

    // Sign the message and POST it to login as admin
    let admin_signed: Signature = admin_wallet
        .sign_message(&admin_login_message)
        .await
        .unwrap();
    info!(?admin_signed);

    let admin_post_login_data = PostLogin {
        msg: admin_login_message,
        sig: admin_signed.to_string(),
        referral_code: None,
    };
    info!(?admin_post_login_data);

    let admin_login_response = r
        .post(&login_post_url)
        .json(&admin_post_login_data)
        .send()
        .await
        .unwrap()
        .json::<LoginPostResponse>()
        .await
        .unwrap();
    info!(?admin_login_response);

    info!("Increasing balance");
    // Login the user
    // Use the bearer token of admin to increase user balance
    let increase_balance_data = AdminIncreaseBalancePost {
        user_address: user_wallet.address(), // set user address to increase balance
        amount: Decimal::from(100),          // set amount to increase
        note: Some("Test increasing balance".to_string()),
    };
    info!(?increase_balance_post_url);
    info!(?increase_balance_data);
    info!(?admin_login_response.bearer_token);

    let increase_balance_response = r
        .post(increase_balance_post_url)
        .json(&increase_balance_data)
        .bearer_auth(admin_login_response.bearer_token)
        .send()
        .await
        .unwrap();
    info!("bug is on the line above. it never returns");
    info!(?increase_balance_response, "http response");

    let increase_balance_response = increase_balance_response
        .json::<serde_json::Value>()
        .await
        .unwrap();
    info!(?increase_balance_response, "json response");

    // Check if the response is as expected
    // TODO: assert_eq!(increase_balance_response["user"], user_wallet.address());
    assert_eq!(
        Decimal::from_str(increase_balance_response["amount"].as_str().unwrap()).unwrap(),
        Decimal::from(100)
    );

    x.wait().await;
}

// #[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[ignore = "under construction"]
#[test_log::test(tokio::test)]
async fn test_admin_change_user_tier() {
    let x = TestApp::spawn(true).await;
    todo!();
}
