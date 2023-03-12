use std::thread::spawn;
use std::time::Duration;
use std::{fs, sync::Arc};

use anyhow::{Context, Result};
use axum::http::HeaderValue;
use futures::FutureExt;
use hyper::header::CONTENT_TYPE;
use hyper::Method;
use kld::api::bind_api_server;
use kld::api::MacaroonAuth;
use logger::KldLogger;
use once_cell::sync::Lazy;
use reqwest::RequestBuilder;
use reqwest::StatusCode;
use serde::Serialize;
use settings::Settings;
use test_utils::ports::get_available_port;
use test_utils::{https_client, TestSettingsBuilder};

use api::{
    routes, Address, Channel, ChannelFee, FundChannel, FundChannelResponse, GetInfo, NewAddress,
    NewAddressResponse, Node, Peer, SetChannelFeeResponse, WalletBalance, WalletTransfer,
    WalletTransferResponse,
};
use tokio::runtime::Runtime;
use tokio::sync::RwLock;

use crate::mocks::mock_lightning::MockLightning;
use crate::mocks::mock_wallet::MockWallet;
use crate::mocks::{TEST_ADDRESS, TEST_ALIAS, TEST_PUBLIC_KEY, TEST_SHORT_CHANNEL_ID};
use crate::quit_signal;

#[tokio::test(flavor = "multi_thread")]
pub async fn test_unauthorized() -> Result<()> {
    let settings = create_api_server().await?;
    assert_eq!(
        StatusCode::UNAUTHORIZED,
        unauthorized_request(&settings, Method::GET, routes::ROOT)
            .send()
            .await?
            .status()
    );
    assert_eq!(
        StatusCode::UNAUTHORIZED,
        unauthorized_request(&settings, Method::GET, routes::GET_INFO)
            .send()
            .await?
            .status()
    );
    assert_eq!(
        StatusCode::UNAUTHORIZED,
        unauthorized_request(&settings, Method::GET, routes::GET_BALANCE)
            .send()
            .await?
            .status()
    );
    assert_eq!(
        StatusCode::UNAUTHORIZED,
        unauthorized_request(&settings, Method::GET, routes::LIST_CHANNELS)
            .send()
            .await?
            .status()
    );
    assert_eq!(
        StatusCode::UNAUTHORIZED,
        unauthorized_request(&settings, Method::POST, routes::OPEN_CHANNEL)
            .send()
            .await?
            .status()
    );
    assert_eq!(
        StatusCode::UNAUTHORIZED,
        readonly_request_with_body(
            &settings,
            Method::POST,
            routes::OPEN_CHANNEL,
            fund_channel_request
        )?
        .send()
        .await?
        .status()
    );
    assert_eq!(
        StatusCode::UNAUTHORIZED,
        unauthorized_request(&settings, Method::POST, routes::SET_CHANNEL_FEE,)
            .send()
            .await?
            .status()
    );
    assert_eq!(
        StatusCode::UNAUTHORIZED,
        readonly_request_with_body(
            &settings,
            Method::POST,
            routes::SET_CHANNEL_FEE,
            set_channel_fee_request
        )?
        .send()
        .await?
        .status()
    );
    assert_eq!(
        StatusCode::UNAUTHORIZED,
        unauthorized_request(&settings, Method::DELETE, routes::CLOSE_CHANNEL,)
            .send()
            .await?
            .status()
    );
    assert_eq!(
        StatusCode::UNAUTHORIZED,
        readonly_request(
            &settings,
            Method::DELETE,
            &routes::CLOSE_CHANNEL.replace(":id", &TEST_SHORT_CHANNEL_ID.to_string()),
        )?
        .send()
        .await?
        .status()
    );
    assert_eq!(
        StatusCode::UNAUTHORIZED,
        unauthorized_request(&settings, Method::POST, routes::WITHDRAW)
            .send()
            .await?
            .status()
    );
    assert_eq!(
        StatusCode::UNAUTHORIZED,
        readonly_request_with_body(&settings, Method::POST, routes::WITHDRAW, withdraw_request)?
            .send()
            .await?
            .status()
    );
    assert_eq!(
        StatusCode::UNAUTHORIZED,
        readonly_request_with_body(
            &settings,
            Method::GET,
            routes::NEW_ADDR,
            NewAddress::default
        )?
        .send()
        .await?
        .status()
    );
    assert_eq!(
        StatusCode::UNAUTHORIZED,
        readonly_request_with_body(
            &settings,
            Method::GET,
            routes::NEW_ADDR,
            NewAddress::default
        )?
        .send()
        .await?
        .status()
    );
    assert_eq!(
        StatusCode::UNAUTHORIZED,
        unauthorized_request(&settings, Method::GET, routes::LIST_PEERS)
            .send()
            .await?
            .status()
    );
    assert_eq!(
        StatusCode::UNAUTHORIZED,
        unauthorized_request(&settings, Method::POST, routes::CONNECT_PEER)
            .send()
            .await?
            .status()
    );
    assert_eq!(
        StatusCode::UNAUTHORIZED,
        readonly_request_with_body(&settings, Method::POST, routes::CONNECT_PEER, || {
            TEST_ADDRESS
        })?
        .send()
        .await?
        .status()
    );
    assert_eq!(
        StatusCode::UNAUTHORIZED,
        unauthorized_request(&settings, Method::DELETE, routes::DISCONNECT_PEER)
            .send()
            .await?
            .status()
    );
    assert_eq!(
        StatusCode::UNAUTHORIZED,
        readonly_request(
            &settings,
            Method::DELETE,
            &routes::DISCONNECT_PEER.replace(":id", TEST_PUBLIC_KEY),
        )?
        .send()
        .await?
        .status()
    );
    assert_eq!(
        StatusCode::UNAUTHORIZED,
        unauthorized_request(&settings, Method::GET, routes::LIST_NODES)
            .send()
            .await?
            .status()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_not_found() -> Result<()> {
    let settings = create_api_server().await?;
    assert_eq!(
        StatusCode::NOT_FOUND,
        admin_request(&settings, Method::GET, "/x")?
            .send()
            .await?
            .status()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_root_readonly() -> Result<()> {
    let settings = create_api_server().await?;
    assert!(readonly_request(&settings, Method::GET, routes::ROOT)?
        .send()
        .await?
        .status()
        .is_success());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_root_admin() -> Result<()> {
    let settings = create_api_server().await?;
    assert!(admin_request(&settings, Method::GET, routes::ROOT)?
        .send()
        .await?
        .status()
        .is_success());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_get_info_readonly() -> Result<()> {
    let settings = create_api_server().await?;
    let info: GetInfo = readonly_request(&settings, Method::GET, routes::GET_INFO)?
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(LIGHTNING.num_peers, info.num_peers);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_get_balance_readonly() -> Result<()> {
    let settings = create_api_server().await?;
    let balance: WalletBalance = readonly_request(&settings, Method::GET, routes::GET_BALANCE)?
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(9, balance.total_balance);
    assert_eq!(4, balance.conf_balance);
    assert_eq!(5, balance.unconf_balance);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_list_channels_readonly() -> Result<()> {
    let settings = create_api_server().await?;
    let channels: Vec<Channel> = readonly_request(&settings, Method::GET, routes::LIST_CHANNELS)?
        .send()
        .await?
        .json()
        .await?;
    let channel = channels.get(0).unwrap();
    assert_eq!(
        "0202755b475334bd9a56a317fd23dfe264b193bcbd7322faa3e974031704068266",
        channel.id
    );
    assert_eq!("true", channel.connected);
    assert_eq!("usable", channel.state);
    assert_eq!(TEST_SHORT_CHANNEL_ID.to_string(), channel.short_channel_id);
    assert_eq!(
        "0000000000000000000000000000000000000000000000000000000000000000",
        channel.funding_txid
    );
    assert_eq!("false", channel.private);
    assert_eq!("100000", channel.msatoshi_to_us);
    assert_eq!("1000000", channel.msatoshi_total);
    assert_eq!("200000", channel.msatoshi_to_them);
    assert_eq!("5000", channel.their_channel_reserve_satoshis);
    assert_eq!("10000", channel.our_channel_reserve_satoshis);
    assert_eq!("100000", channel.spendable_msatoshi);
    assert_eq!(1, channel.direction);
    assert_eq!(TEST_ALIAS, channel.alias);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_open_channel_admin() -> Result<()> {
    let settings = create_api_server().await?;
    let response: FundChannelResponse = admin_request_with_body(
        &settings,
        Method::POST,
        routes::OPEN_CHANNEL,
        fund_channel_request,
    )?
    .send()
    .await?
    .json()
    .await?;
    assert_eq!(
        "fba98a9a61ef62c081b31769f66a81f1640b4f94d48b550a550034cb4990eded",
        response.txid
    );
    assert_eq!(
        "0101010101010101010101010101010101010101010101010101010101010101",
        response.channel_id
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_set_channel_fee_admin() -> Result<()> {
    let settings = create_api_server().await?;
    let response: SetChannelFeeResponse = admin_request_with_body(
        &settings,
        Method::POST,
        routes::SET_CHANNEL_FEE,
        set_channel_fee_request,
    )?
    .send()
    .await?
    .json()
    .await?;

    let fee = response.0.get(0).context("Bad response")?;
    assert_eq!(TEST_SHORT_CHANNEL_ID.to_string(), fee.short_channel_id);
    assert_eq!(TEST_PUBLIC_KEY, fee.peer_id);
    assert_eq!(set_channel_fee_request().base, Some(fee.base));
    assert_eq!(set_channel_fee_request().ppm, Some(fee.ppm));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_set_all_channel_fees_admin() -> Result<()> {
    let settings = create_api_server().await?;
    let request = ChannelFee {
        id: "all".to_string(),
        base: Some(32500),
        ppm: Some(1200),
    };
    let response: SetChannelFeeResponse =
        admin_request_with_body(&settings, Method::POST, routes::SET_CHANNEL_FEE, || {
            request.clone()
        })?
        .send()
        .await?
        .json()
        .await?;

    let fee = response.0.get(0).context("Bad response")?;
    assert_eq!(TEST_SHORT_CHANNEL_ID.to_string(), fee.short_channel_id);
    assert_eq!(TEST_PUBLIC_KEY, fee.peer_id);
    assert_eq!(request.base, Some(fee.base));
    assert_eq!(request.ppm, Some(fee.ppm));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_close_channel_admin() -> Result<()> {
    let settings = create_api_server().await?;
    let result = admin_request(
        &settings,
        Method::DELETE,
        &routes::CLOSE_CHANNEL.replace(":id", &TEST_SHORT_CHANNEL_ID.to_string()),
    )?
    .send()
    .await?;
    assert!(result.status().is_success());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_withdraw_admin() -> Result<()> {
    let settings = create_api_server().await?;
    let response: WalletTransferResponse =
        admin_request_with_body(&settings, Method::POST, routes::WITHDRAW, withdraw_request)?
            .send()
            .await?
            .json()
            .await?;
    assert_eq!("{\"version\":2,\"lock_time\":0,\"input\":[{\"previous_output\":\"0f60fdd185542f2c6ea19030b0796051e7772b6026dd5ddccd7a2f93b73e6fc2:1\",\"script_sig\":\"\",\"sequence\":4294967295,\"witness\":[]},{\"previous_output\":\"0f60fdd185542f2c6ea19030b0796051e7772b6026dd5ddccd7a2f93b73e6fc2:0\",\"script_sig\":\"\",\"sequence\":4294967295,\"witness\":[]},{\"previous_output\":\"0e53ec5dfb2cb8a71fec32dc9a634a35b7e24799295ddd5278217822e0b31f57:5\",\"script_sig\":\"\",\"sequence\":4294967295,\"witness\":[]}],\"output\":[{\"value\":1000,\"script_pubkey\":\"aaee\"},{\"value\":1000,\"script_pubkey\":\"aa\"},{\"value\":800,\"script_pubkey\":\"ff\"}]}", response.tx);
    assert_eq!(
        "fba98a9a61ef62c081b31769f66a81f1640b4f94d48b550a550034cb4990eded",
        response.txid
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_new_address_admin() -> Result<()> {
    let settings = create_api_server().await?;
    let response: NewAddressResponse = admin_request_with_body(
        &settings,
        Method::GET,
        routes::NEW_ADDR,
        NewAddress::default,
    )?
    .send()
    .await?
    .json()
    .await?;
    assert_eq!(TEST_ADDRESS.to_string(), response.address);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_list_peers_readonly() -> Result<()> {
    let settings = create_api_server().await?;
    let response: Vec<Peer> = readonly_request(&settings, Method::GET, routes::LIST_PEERS)?
        .send()
        .await?
        .json()
        .await?;
    let netaddr = Some(Address {
        address_type: "ipv4".to_string(),
        address: "127.0.0.1".to_string(),
        port: 5555,
    });
    assert!(response.contains(&Peer {
        id: TEST_PUBLIC_KEY.to_string(),
        connected: true,
        netaddr,
        alias: TEST_ALIAS.to_string()
    }));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_connect_peer_admin() -> Result<()> {
    let settings = create_api_server().await?;
    let response: String =
        admin_request_with_body(&settings, Method::POST, routes::CONNECT_PEER, || {
            TEST_PUBLIC_KEY
        })?
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(TEST_PUBLIC_KEY, response);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_disconnect_peer_admin() -> Result<()> {
    let settings = create_api_server().await?;
    let response = admin_request(
        &settings,
        Method::DELETE,
        &routes::DISCONNECT_PEER.replace(":id", TEST_PUBLIC_KEY),
    )?
    .send()
    .await?;
    assert!(response.status().is_success());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_disconnect_peer_admin_malformed_key() -> Result<()> {
    let settings = create_api_server().await?;
    let response: api::Error = admin_request(
        &settings,
        Method::DELETE,
        &routes::DISCONNECT_PEER.replace(":id", "abcd"),
    )?
    .send()
    .await?
    .json()
    .await?;
    assert_eq!(response.status, StatusCode::BAD_REQUEST.to_string());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_get_node_readonly() -> Result<()> {
    let settings = create_api_server().await?;
    let nodes: Vec<Node> = readonly_request(
        &settings,
        Method::GET,
        &routes::LIST_NODE.replace(":id", TEST_PUBLIC_KEY),
    )?
    .send()
    .await?
    .json()
    .await?;
    let node = nodes.get(0).context("no node in response")?;
    assert_eq!(TEST_PUBLIC_KEY, node.node_id);
    assert_eq!(TEST_ALIAS, node.alias);
    assert_eq!("010203", node.color);
    assert_eq!(21000000, node.last_timestamp);
    assert!(node.addresses.contains(&Address {
        address_type: "ipv4".to_string(),
        address: "127.0.0.1".to_string(),
        port: 5555
    }));
    assert!(!node.features.is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_list_nodes_readonly() -> Result<()> {
    let settings = create_api_server().await?;
    let nodes: Vec<Node> = readonly_request(&settings, Method::GET, routes::LIST_NODES)?
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(TEST_PUBLIC_KEY, nodes.get(0).context("bad result")?.node_id);
    Ok(())
}

fn withdraw_request() -> WalletTransfer {
    WalletTransfer {
        address: TEST_ADDRESS.to_string(),
        satoshis: "all".to_string(),
        fee_rate: None,
        min_conf: Some("3".to_string()),
        utxos: vec![],
    }
}

fn fund_channel_request() -> FundChannel {
    FundChannel {
        id: TEST_PUBLIC_KEY.to_string() + "@1.2.3.4:1234",
        satoshis: "21000000".to_string(),
        fee_rate: Some("4".to_string()),
        announce: Some("true".to_string()),
        push_msat: Some("10000".to_string()),
        close_to: None,
        request_amt: None,
        compact_lease: None,
        min_conf: Some(5),
        utxos: vec![],
    }
}

fn set_channel_fee_request() -> ChannelFee {
    ChannelFee {
        id: TEST_SHORT_CHANNEL_ID.to_string(),
        base: Some(32500),
        ppm: Some(1200),
    }
}

static API_RUNTIME: Lazy<Runtime> = Lazy::new(|| Runtime::new().unwrap());

static API_SETTINGS: Lazy<RwLock<Option<Settings>>> = Lazy::new(|| RwLock::new(None));

pub async fn create_api_server() -> Result<Settings> {
    let mut settings = API_SETTINGS.write().await;
    if settings.is_some() {
        drop(settings); // release lock
        return Ok(API_SETTINGS.read().await.as_ref().unwrap().clone());
    }
    KldLogger::init("test", log::LevelFilter::Info);
    let rest_api_port = get_available_port().context("no port available")?;
    let rest_api_address = format!("127.0.0.1:{rest_api_port}");
    let s = TestSettingsBuilder::new()
        .with_data_dir(&format!("{}/test_api", env!("CARGO_TARGET_TMPDIR")))
        .with_rest_api_address(rest_api_address.clone())
        .build();
    let certs_dir = s.certs_dir.clone();
    let macaroon_auth = Arc::new(
        MacaroonAuth::init(&[0u8; 32], &s.data_dir).context("cannot initialize macaroon auth")?,
    );

    // Run the API with its own runtime in its own thread.
    spawn(move || {
        API_RUNTIME
            .block_on(async {
                bind_api_server(rest_api_address, certs_dir)
                    .await?
                    .serve(
                        LIGHTNING.clone(),
                        Arc::new(MockWallet::default()),
                        macaroon_auth,
                        quit_signal().shared(),
                    )
                    .await
            })
            .unwrap()
    });

    while !readonly_request(&s, Method::GET, routes::ROOT)?
        .send()
        .await?
        .status()
        .is_success()
    {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    *settings = Some(s);
    drop(settings); // release lock
    Ok(API_SETTINGS.read().await.as_ref().unwrap().clone())
}

// TODO: those should be read only once when parsing settings...
pub fn admin_macaroon(settings: &Settings) -> Result<Vec<u8>> {
    let path = format!("{}/macaroons/admin.macaroon", settings.data_dir);
    fs::read(&path).with_context(|| format!("Failed to read {path}"))
}

pub fn readonly_macaroon(settings: &Settings) -> Result<Vec<u8>> {
    let path = format!("{}/macaroons/readonly.macaroon", settings.data_dir);
    fs::read(&path).with_context(|| format!("Failed to read {path}"))
}

static LIGHTNING: Lazy<Arc<MockLightning>> = Lazy::new(|| Arc::new(MockLightning::default()));

fn unauthorized_request(settings: &Settings, method: Method, route: &str) -> RequestBuilder {
    let address = &settings.rest_api_address;
    https_client()
        .request(method, format!("https://{address}{route}"))
        .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
}

fn admin_request(settings: &Settings, method: Method, route: &str) -> Result<RequestBuilder> {
    Ok(unauthorized_request(settings, method, route).header("macaroon", admin_macaroon(settings)?))
}

fn admin_request_with_body<T: Serialize, F: FnOnce() -> T>(
    settings: &Settings,
    method: Method,
    route: &str,
    f: F,
) -> Result<RequestBuilder> {
    let body = serde_json::to_string(&f()).unwrap();
    Ok(admin_request(settings, method, route)?.body(body))
}

fn readonly_request(settings: &Settings, method: Method, route: &str) -> Result<RequestBuilder> {
    Ok(unauthorized_request(settings, method, route)
        .header("macaroon", readonly_macaroon(settings)?))
}

fn readonly_request_with_body<T: Serialize, F: FnOnce() -> T>(
    settings: &Settings,
    method: Method,
    route: &str,
    f: F,
) -> Result<RequestBuilder> {
    let body = serde_json::to_string(&f()).unwrap();
    Ok(readonly_request(settings, method, route)?.body(body))
}
