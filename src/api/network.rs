use api::{Address, Node};
use axum::{extract::Path, response::IntoResponse, Extension, Json};
use bitcoin::secp256k1::PublicKey;
use hex::ToHex;
use lightning::{
    ln::msgs::NetAddress,
    routing::gossip::{NodeId, NodeInfo},
};
use std::{
    net::{Ipv4Addr, Ipv6Addr},
    str::FromStr,
    sync::Arc,
};

use super::{bad_request, unauthorized, ApiError, KldMacaroon, LightningInterface, MacaroonAuth};

pub(crate) async fn list_nodes(
    macaroon: KldMacaroon,
    Extension(macaroon_auth): Extension<Arc<MacaroonAuth>>,
    Extension(lightning_interface): Extension<Arc<dyn LightningInterface + Send + Sync>>,
) -> Result<impl IntoResponse, ApiError> {
    macaroon_auth
        .verify_readonly_macaroon(&macaroon.0)
        .map_err(unauthorized)?;
    let nodes: Vec<Node> = lightning_interface
        .nodes()
        .unordered_iter()
        .filter_map(|(node_id, announcement)| to_api_node(node_id, announcement))
        .collect();
    Ok(Json(nodes))
}

pub(crate) async fn get_node(
    macaroon: KldMacaroon,
    Extension(macaroon_auth): Extension<Arc<MacaroonAuth>>,
    Extension(lightning_interface): Extension<Arc<dyn LightningInterface + Send + Sync>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    macaroon_auth
        .verify_readonly_macaroon(&macaroon.0)
        .map_err(unauthorized)?;
    let public_key = PublicKey::from_str(&id).map_err(bad_request)?;
    let node_id = NodeId::from_pubkey(&public_key);
    if let Some(node_info) = lightning_interface.get_node(&node_id) {
        if let Some(node) = to_api_node(&node_id, &node_info) {
            return Ok(Json(vec![node]));
        }
    }
    Err(ApiError::NotFound(id))
}

fn to_api_node(node_id: &NodeId, node_info: &NodeInfo) -> Option<Node> {
    node_info.announcement_info.as_ref().map(|n| Node {
        node_id: node_id.as_slice().encode_hex(),
        alias: n.alias.to_string(),
        color: n.rgb.encode_hex(),
        last_timestamp: n.last_update,
        features: n.features.to_string(),
        addresses: n.addresses.iter().map(to_api_address).collect(),
    })
}

pub(crate) fn to_api_address(net_address: &NetAddress) -> Address {
    match net_address {
        NetAddress::IPv4 { addr, port } => Address {
            address_type: "ipv4".to_string(),
            address: Ipv4Addr::from(*addr).to_string(),
            port: *port,
        },
        NetAddress::IPv6 { addr, port } => Address {
            address_type: "ipv6".to_string(),
            address: Ipv6Addr::from(*addr).to_string(),
            port: *port,
        },
        NetAddress::OnionV2(pubkey) => Address {
            address_type: "onionv2".to_string(),
            address: pubkey.encode_hex(),
            port: 0,
        },
        NetAddress::OnionV3 {
            ed25519_pubkey,
            checksum: _,
            version: _,
            port,
        } => Address {
            address_type: "onionv3".to_string(),
            address: ed25519_pubkey.encode_hex(),
            port: *port,
        },
        NetAddress::Hostname { hostname, port } => Address {
            address_type: "hostname".to_string(),
            address: hostname.to_string(),
            port: *port,
        },
    }
}
