//! Logic for onboarding a new `pd` node onto an existing testnet.
//! Handles generation of config files for `pd` and `tendermint`.
use anyhow::Context;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use tendermint_config::net::Address as TendermintAddress;
use url::Url;

use crate::testnet::{
    generate_tm_config, parse_tm_address, parse_tm_address_listener, write_configs, ValidatorKeys,
};

/// Bootstrap a connection to a testnet, via a node on that testnet.
/// Look up network peer info from the target node, and seed the tendermint
/// p2p settings with that peer info.
pub async fn testnet_join(
    output_dir: PathBuf,
    node: Url,
    node_name: &str,
    external_address: Option<TendermintAddress>,
    tm_rpc_bind: SocketAddr,
    tm_p2p_bind: SocketAddr,
) -> anyhow::Result<()> {
    let mut node_dir = output_dir;
    node_dir.push("node0");
    let genesis_url = node.join("/genesis")?;
    tracing::info!(?genesis_url, "fetching genesis");
    // We need to download the genesis data and the node ID from the remote node.
    // TODO: replace with TendermintProxyServiceClient
    let client = reqwest::Client::new();
    let genesis_json = client
        .get(genesis_url)
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?
        .get_mut("result")
        .and_then(|v| v.get_mut("genesis"))
        .ok_or_else(|| anyhow::anyhow!("could not parse JSON from response"))?
        .take();
    let genesis = serde_json::value::from_value(genesis_json)?;
    tracing::info!("fetched genesis");

    // Look up more peers from the target node, so that generated tendermint config
    // contains multiple addresses, making peering easier.
    let mut peers = Vec::new();
    if let Some(node_tm_address) = fetch_listen_address(&node).await {
        peers.push(node_tm_address);
    } else {
        // We consider it odd that a bootstrap node has a remotely accessible
        // RPC endpoint, but no P2P listener enabled.
        tracing::warn!("Failed to find listenaddr for {}", &node);
    }
    let new_peers = fetch_peers(&node).await?;
    peers.extend(new_peers);
    tracing::info!(?peers);

    let tm_config = generate_tm_config(
        node_name,
        peers,
        external_address,
        Some(tm_rpc_bind),
        Some(tm_p2p_bind),
    )?;

    let vk = ValidatorKeys::generate();
    write_configs(node_dir, &vk, &genesis, tm_config)?;
    Ok(())
}

/// Query the Tendermint node's RPC endpoint at `tm_url` and return the listener
/// address for the P2P endpoint. Returns an [Option<TendermintAddress>] because
/// it's possible that no p2p listener is configured.
pub async fn fetch_listen_address(tm_url: &Url) -> Option<TendermintAddress> {
    let client = reqwest::Client::new();
    // We cannot assume the RPC URL is the same as the P2P address,
    // so we use the RPC URL to look up the P2P listening address.
    let listen_info = client
        .get(tm_url.join("net_info").ok()?)
        .send()
        .await
        .ok()?
        .json::<serde_json::Value>()
        .await
        .ok()?
        .get_mut("result")
        .and_then(|v| v.get_mut("listeners"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    tracing::debug!(?listen_info, "found listener info from bootstrap node");
    let first_entry = listen_info[0].as_str().unwrap_or_default();
    let listen_addr = parse_tm_address_listener(first_entry)?;

    // Next we'll look up the node_id, so we can assemble a self-authenticating
    // Tendermint Address, in the form of <id>@<url>.
    let node_id = client
        .get(tm_url.join("/status").ok()?)
        .send()
        .await
        .ok()?
        .json::<serde_json::Value>()
        .await
        .ok()?
        .get_mut("result")
        .and_then(|v| v.get_mut("node_info"))
        .and_then(|v| v.get_mut("id"))
        .ok_or_else(|| anyhow::anyhow!("could not parse JSON from response"))
        .ok()?
        .take();

    let node_id: tendermint::node::Id = serde_json::value::from_value(node_id).ok()?;
    tracing::debug!(?node_id, "fetched node id");

    let listen_addr_url = Url::parse(&format!("{}", &listen_addr)).ok()?;
    tracing::info!(
        ?listen_addr_url,
        "fetched listener address for bootstrap node"
    );
    parse_tm_address(Some(&node_id), &listen_addr_url).ok()
}

/// Query the Tendermint node's RPC endpoint at `tm_url` and return a list
/// of all known peers by their `external_address`es. Omits private/special
/// addresses like `localhost` or `0.0.0.0`.
pub async fn fetch_peers(tm_url: &Url) -> anyhow::Result<Vec<TendermintAddress>> {
    let client = reqwest::Client::new();
    let net_info_url = tm_url.join("/net_info")?;
    let net_info_peers = client
        .get(net_info_url)
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?
        .get("result")
        .and_then(|v| v.get("peers"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut peers = Vec::new();
    for raw_peer in net_info_peers {
        let node_id: tendermint::node::Id = raw_peer
            .get("node_info")
            .and_then(|v| v.get("id"))
            .and_then(|v| serde_json::value::from_value(v.clone()).ok())
            .ok_or_else(|| anyhow::anyhow!("Could not parse node_info.id from JSON response"))?;

        let listen_addr = raw_peer
            .get("node_info")
            .and_then(|v| v.get("listen_addr"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!("Could not parse node_info.listen_addr from JSON response")
            })?;

        // Filter out addresses that are obviously not external addresses.
        if !address_could_be_external(listen_addr) {
            continue;
        }

        // The API returns a str formatted as a SocketAddr; prepend protocol so we can handle
        // as a URL. The Tendermint config template already includes the tcp:// prefix.
        let laddr = format!("tcp://{}", listen_addr);
        let listen_url = Url::parse(&laddr).context(format!(
            "Failed to parse candidate tendermint addr as URL: {}",
            listen_addr
        ))?;
        let peer_tm_address = parse_tm_address(Some(&node_id), &listen_url)?;
        peers.push(peer_tm_address);
    }
    Ok(peers)
}

/// Check whether SocketAddress spec is likely to be externally-accessible.
/// Filters out RFC1918 and loopback addresses. Requires an address and port.
// TODO: This should return a Result, to be clearer about the expectation
// of a SocketAddr, rather than an IpAddr, as arg.
fn address_could_be_external(address: &str) -> bool {
    let addr = address.parse::<SocketAddr>().ok();
    match addr {
        Some(a) => match a.ip() {
            IpAddr::V4(ip) => !(ip.is_private() || ip.is_loopback() || ip.is_unspecified()),
            IpAddr::V6(ip) => !(ip.is_loopback() || ip.is_unspecified()),
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn external_address_detection() {
        assert!(!address_could_be_external("127.0.0.1"));
        assert!(!address_could_be_external("0.0.0.0"));
        assert!(!address_could_be_external("0.0.0.0:80"));
        assert!(!address_could_be_external("192.168.4.1:26657"));
        // Real GCP IPv4 address, used for `testnet.penumbra.zone` 2023Q1.
        assert!(!address_could_be_external("35.226.255.25"));
        assert!(address_could_be_external("35.226.255.25:26657"));
    }
}
