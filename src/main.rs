use std::{
    collections::{BTreeMap, BTreeSet},
    convert::TryInto,
    fs,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use adnl::{
    DhtNode,
    node::{AdnlNode, AdnlNodeConfig},
};
use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use ever_block::{Ed25519KeyOption, KeyId, KeyOption, UInt256, base64_decode, base64_encode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::timeout;
use ton_api::{
    IntoBoxed,
    ton::{
        PublicKey,
        adnl::{Address, address::address::Udp, addresslist::AddressList as AdnlAddressList},
        dht::node::Node as DhtNodeConfig,
        pub_::publickey::Ed25519,
    },
};

const DHT_KEY_TAG: usize = 1;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Build(BuildArgs),
}

#[derive(Parser)]
struct BuildArgs {
    #[arg(short, long, default_value = "everscale_global_config_builder.json")]
    config: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
struct AppConfig {
    seed_global_config_path: PathBuf,
    output_global_config_path: PathBuf,
    report_path: PathBuf,
    #[serde(default = "default_local_adnl_addr")]
    local_adnl_addr: String,
    #[serde(default = "default_crawl_rounds")]
    crawl_rounds: usize,
    #[serde(default = "default_peer_timeout_secs")]
    peer_timeout_secs: u64,
    #[serde(default = "default_max_known_nodes")]
    max_known_nodes: usize,
    #[serde(default = "default_max_output_nodes")]
    max_output_nodes: usize,
    #[serde(default = "default_min_successes")]
    min_successes: u32,
    #[serde(default)]
    include_seed_nodes: bool,
    #[serde(default)]
    allow_private_ips: bool,
    #[serde(default)]
    compact: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TonNodeGlobalConfigJson {
    dht: DhtGlobalConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DhtGlobalConfig {
    static_nodes: DhtNodes,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DhtNodes {
    nodes: Vec<ConfigDhtNode>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ConfigDhtNode {
    id: ConfigDhtNodeId,
    addr_list: ConfigAddressList,
    version: Option<i32>,
    signature: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ConfigDhtNodeId {
    #[serde(alias = "@type")]
    type_node: Option<String>,
    key: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ConfigAddressList {
    addrs: Vec<ConfigAddress>,
    version: Option<i32>,
    reinit_date: Option<i32>,
    priority: Option<i32>,
    expire_at: Option<i32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ConfigAddress {
    #[serde(alias = "@type")]
    type_node: Option<String>,
    ip: Option<i64>,
    port: Option<i32>,
}

struct Crawler {
    _adnl: Arc<AdnlNode>,
    dht: Arc<DhtNode>,
    peer_timeout: Duration,
    max_known_nodes: usize,
}

#[derive(Clone)]
struct Candidate {
    node: DhtNodeConfig,
    key_id: Arc<KeyId>,
    public_key: String,
    first_seen_at: u64,
    last_seen_at: u64,
    seen_count: u32,
    success_count: u32,
    seed: bool,
    last_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct BuildReport {
    schema_version: u32,
    generated_at: u64,
    seed_global_config_path: String,
    output_global_config_path: String,
    seed_nodes: usize,
    discovered_nodes: usize,
    included_nodes: usize,
    excluded_nodes: usize,
    summary: BuildSummary,
    nodes: Vec<ReportNode>,
}

#[derive(Debug, Serialize)]
struct BuildSummary {
    crawl_rounds: usize,
    min_successes: u32,
    include_seed_nodes: bool,
    allow_private_ips: bool,
    max_output_nodes: usize,
}

#[derive(Debug, Serialize)]
struct ReportNode {
    key_id: String,
    public_key: String,
    ip: Option<String>,
    port: Option<i32>,
    seed: bool,
    included: bool,
    reason: String,
    seen_count: u32,
    success_count: u32,
    first_seen_at: u64,
    last_seen_at: u64,
    version: i32,
    expire_at: i32,
    last_error: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Commands::Build(args) => build(args).await,
    }
}

async fn build(args: BuildArgs) -> Result<()> {
    let base_dir = config_base_dir(&args.config);
    let mut config: AppConfig = read_json(&args.config)
        .with_context(|| format!("failed to read config {}", args.config.display()))?;
    config.resolve_paths(&base_dir);

    let now = unix_now();
    let seed_json = read_json_value(&config.seed_global_config_path)?;
    let seed_config: TonNodeGlobalConfigJson = serde_json::from_value(seed_json.clone())
        .with_context(|| {
            format!(
                "failed to parse {}",
                config.seed_global_config_path.display()
            )
        })?;
    let seed_nodes = seed_config.get_dht_nodes_configs()?;
    if seed_nodes.is_empty() {
        bail!("seed config has no valid DHT static nodes");
    }

    eprintln!(
        "build start seed_nodes={} rounds={} local_adnl={}",
        seed_nodes.len(),
        config.crawl_rounds,
        config.local_adnl_addr
    );

    let crawler = Crawler::new(
        &config.local_adnl_addr,
        &seed_nodes,
        Duration::from_secs(config.peer_timeout_secs),
        config.max_known_nodes,
    )
    .await?;

    let mut candidates = BTreeMap::new();
    for node in &seed_nodes {
        record_candidate(&mut candidates, node.clone(), true, now)?;
    }
    record_known_nodes(&crawler, &mut candidates, now)?;

    for round in 1..=config.crawl_rounds {
        let keys = candidate_keys(&candidates);
        let mut queried = 0usize;
        let mut ok = 0usize;
        for key in keys {
            queried += 1;
            match crawler.expand_peer(&key).await {
                Ok(true) => {
                    ok += 1;
                    if let Some(candidate) = find_candidate_by_key_mut(&mut candidates, &key) {
                        candidate.success_count = candidate.success_count.saturating_add(1);
                        candidate.last_error = None;
                    }
                }
                Ok(false) => {
                    if let Some(candidate) = find_candidate_by_key_mut(&mut candidates, &key) {
                        candidate.last_error = Some("peer returned no DHT data".to_owned());
                    }
                }
                Err(error) => {
                    if let Some(candidate) = find_candidate_by_key_mut(&mut candidates, &key) {
                        candidate.last_error = Some(error.to_string());
                    }
                }
            }
        }
        record_known_nodes(&crawler, &mut candidates, now)?;
        eprintln!(
            "round {round} queried={queried} responsive={ok} known={}",
            candidates.len()
        );
    }

    validate_candidates(&crawler, &mut candidates).await;

    let mut report_nodes = Vec::new();
    let mut output_nodes = Vec::new();
    for candidate in candidates.values() {
        let decision = inclusion_decision(candidate, &config, now);
        if decision.included {
            output_nodes.push(candidate.clone());
        }
        report_nodes.push(candidate_report(candidate, decision));
    }
    output_nodes.sort_by(|a, b| {
        b.success_count
            .cmp(&a.success_count)
            .then_with(|| b.seen_count.cmp(&a.seen_count))
            .then_with(|| a.public_key.cmp(&b.public_key))
    });
    output_nodes.truncate(config.max_output_nodes);

    let output_values = output_nodes
        .iter()
        .map(|candidate| dht_node_to_json(&candidate.node))
        .collect::<Result<Vec<_>>>()?;
    let mut output_config = seed_json;
    set_output_dht_nodes(&mut output_config, output_values)?;
    write_json_value(
        &config.output_global_config_path,
        &output_config,
        config.compact,
    )?;

    let included_keys: BTreeSet<_> = output_nodes
        .iter()
        .map(|candidate| candidate.public_key.as_str())
        .collect();
    for node in &mut report_nodes {
        node.included = included_keys.contains(node.public_key.as_str());
        if !node.included && node.reason == "included" {
            node.reason = "excluded by max_output_nodes limit".to_owned();
        }
    }
    report_nodes.sort_by(|a, b| a.public_key.cmp(&b.public_key));

    let report = BuildReport {
        schema_version: 1,
        generated_at: now,
        seed_global_config_path: config.seed_global_config_path.display().to_string(),
        output_global_config_path: config.output_global_config_path.display().to_string(),
        seed_nodes: seed_nodes.len(),
        discovered_nodes: candidates.len(),
        included_nodes: output_nodes.len(),
        excluded_nodes: candidates.len().saturating_sub(output_nodes.len()),
        summary: BuildSummary {
            crawl_rounds: config.crawl_rounds,
            min_successes: config.min_successes,
            include_seed_nodes: config.include_seed_nodes,
            allow_private_ips: config.allow_private_ips,
            max_output_nodes: config.max_output_nodes,
        },
        nodes: report_nodes,
    };
    write_json(&config.report_path, &report, config.compact)?;

    eprintln!(
        "build ok discovered={} included={} output={}",
        candidates.len(),
        output_nodes.len(),
        config.output_global_config_path.display()
    );

    Ok(())
}

impl AppConfig {
    fn resolve_paths(&mut self, base_dir: &Path) {
        self.seed_global_config_path = resolve_config_path(base_dir, &self.seed_global_config_path);
        self.output_global_config_path =
            resolve_config_path(base_dir, &self.output_global_config_path);
        self.report_path = resolve_config_path(base_dir, &self.report_path);
    }
}

impl Crawler {
    async fn new(
        local_adnl_addr: &str,
        seed_nodes: &[DhtNodeConfig],
        peer_timeout: Duration,
        max_known_nodes: usize,
    ) -> Result<Self> {
        let (_, adnl_config) = AdnlNodeConfig::with_ip_address_and_private_key_tags(
            local_adnl_addr,
            vec![DHT_KEY_TAG],
        )
        .context("failed to create local ADNL config")?;
        let adnl = AdnlNode::with_config(adnl_config)
            .await
            .context("failed to create local ADNL node")?;
        let dht = DhtNode::with_params(adnl.clone(), DHT_KEY_TAG, None)
            .context("failed to create DHT node")?;
        AdnlNode::start(&adnl, vec![dht.clone()])
            .await
            .context("failed to start ADNL node")?;

        let mut added = 0usize;
        for node in seed_nodes {
            if dht
                .add_peer_to_network(node, None)
                .context("failed to add seed DHT peer")?
                .is_some()
            {
                added += 1;
            }
        }
        if added == 0 {
            bail!("seed config has no signed DHT nodes accepted by ever-adnl");
        }

        Ok(Self {
            _adnl: adnl,
            dht,
            peer_timeout,
            max_known_nodes,
        })
    }

    fn known_nodes(&self) -> Result<Vec<DhtNodeConfig>> {
        self.dht
            .get_known_nodes_of_network(self.max_known_nodes, None)
            .context("failed to read known DHT nodes")
    }

    async fn expand_peer(&self, key: &Arc<KeyId>) -> Result<bool> {
        let signed = match timeout(
            self.peer_timeout,
            self.dht.get_signed_address_list_in_network(key, None),
        )
        .await
        {
            Ok(result) => result.context("get_signed_address_list failed")?,
            Err(_) => false,
        };
        let found = match timeout(
            self.peer_timeout,
            self.dht.find_dht_nodes_in_network(key, None),
        )
        .await
        {
            Ok(result) => result.context("find_dht_nodes failed")?,
            Err(_) => false,
        };
        Ok(signed || found)
    }

    async fn ping(&self, key: &Arc<KeyId>) -> Result<bool> {
        match timeout(self.peer_timeout, self.dht.ping_in_network(key, None)).await {
            Ok(result) => result.context("DHT ping failed"),
            Err(_) => Ok(false),
        }
    }
}

impl TonNodeGlobalConfigJson {
    fn get_dht_nodes_configs(&self) -> Result<Vec<DhtNodeConfig>> {
        let mut result = Vec::new();
        for dht_node in &self.dht.static_nodes.nodes {
            let key = dht_node.id.convert_key()?;
            let mut addrs = Vec::new();
            for addr in &dht_node.addr_list.addrs {
                if !addr.is_udp() {
                    continue;
                }
                let Some(ip) = addr.ip else {
                    continue;
                };
                let Some(port) = addr.port else {
                    continue;
                };
                addrs.push(
                    Udp {
                        ip: ip as i32,
                        port,
                    }
                    .into_boxed(),
                );
            }

            let Some(version) = dht_node.addr_list.version else {
                continue;
            };
            let Some(reinit_date) = dht_node.addr_list.reinit_date else {
                continue;
            };
            let Some(priority) = dht_node.addr_list.priority else {
                continue;
            };
            let Some(expire_at) = dht_node.addr_list.expire_at else {
                continue;
            };
            let Some(node_version) = dht_node.version else {
                continue;
            };
            let Some(signature) = &dht_node.signature else {
                continue;
            };

            result.push(DhtNodeConfig {
                id: Ed25519 {
                    key: UInt256::with_array(key.pub_key()?.try_into()?),
                }
                .into_boxed(),
                addr_list: AdnlAddressList {
                    addrs,
                    version,
                    reinit_date,
                    priority,
                    expire_at,
                },
                version: node_version,
                signature: base64_decode(signature)?,
            });
        }
        Ok(result)
    }
}

impl ConfigDhtNodeId {
    fn convert_key(&self) -> Result<Arc<dyn KeyOption>> {
        let type_node = self
            .type_node
            .as_deref()
            .ok_or_else(|| anyhow!("DHT node key type is missing"))?;
        if type_node != "pub.ed25519" {
            bail!("unsupported DHT node key type {type_node}");
        }

        let key = self
            .key
            .as_deref()
            .ok_or_else(|| anyhow!("DHT node public key is missing"))
            .and_then(|key| base64_decode(key).map_err(Into::into))?;
        let pub_key = key
            .get(..32)
            .ok_or_else(|| anyhow!("DHT node public key is shorter than 32 bytes"))?
            .try_into()?;
        Ok(Ed25519KeyOption::from_public_key(pub_key))
    }
}

impl ConfigAddress {
    fn is_udp(&self) -> bool {
        self.type_node.as_deref().unwrap_or("adnl.address.udp") == "adnl.address.udp"
    }
}

struct InclusionDecision {
    included: bool,
    reason: String,
}

fn record_known_nodes(
    crawler: &Crawler,
    candidates: &mut BTreeMap<String, Candidate>,
    now: u64,
) -> Result<()> {
    for node in crawler.known_nodes()? {
        record_candidate(candidates, node, false, now)?;
    }
    Ok(())
}

fn record_candidate(
    candidates: &mut BTreeMap<String, Candidate>,
    node: DhtNodeConfig,
    seed: bool,
    now: u64,
) -> Result<()> {
    let key_id = node_key_id(&node)?;
    let key = key_id.to_string();
    let public_key = node_public_key_base64(&node)?;
    if let Some(candidate) = candidates.get_mut(&key) {
        candidate.node = node;
        candidate.public_key = public_key;
        candidate.last_seen_at = now;
        candidate.seen_count = candidate.seen_count.saturating_add(1);
        candidate.seed |= seed;
        return Ok(());
    }

    candidates.insert(
        key,
        Candidate {
            node,
            key_id,
            public_key,
            first_seen_at: now,
            last_seen_at: now,
            seen_count: 1,
            success_count: 0,
            seed,
            last_error: None,
        },
    );
    Ok(())
}

async fn validate_candidates(crawler: &Crawler, candidates: &mut BTreeMap<String, Candidate>) {
    let keys = candidate_keys(candidates);
    for key in keys {
        let result = crawler.ping(&key).await;
        if let Some(candidate) = find_candidate_by_key_mut(candidates, &key) {
            match result {
                Ok(true) => {
                    candidate.success_count = candidate.success_count.saturating_add(1);
                    candidate.last_error = None;
                }
                Ok(false) => {
                    candidate.last_error = Some("DHT ping returned no response".to_owned());
                }
                Err(error) => {
                    candidate.last_error = Some(error.to_string());
                }
            }
        }
    }
}

fn candidate_keys(candidates: &BTreeMap<String, Candidate>) -> Vec<Arc<KeyId>> {
    candidates
        .values()
        .map(|candidate| candidate.key_id.clone())
        .collect()
}

fn find_candidate_by_key_mut<'a>(
    candidates: &'a mut BTreeMap<String, Candidate>,
    key: &Arc<KeyId>,
) -> Option<&'a mut Candidate> {
    candidates.get_mut(&key.to_string())
}

fn inclusion_decision(candidate: &Candidate, config: &AppConfig, now: u64) -> InclusionDecision {
    let (ip, _port) = match first_udp4_endpoint(&candidate.node) {
        Some(endpoint) => endpoint,
        None => {
            return InclusionDecision {
                included: false,
                reason: "no IPv4 UDP address".to_owned(),
            };
        }
    };

    let expire_at = candidate.node.addr_list.expire_at;
    if expire_at > 0 && expire_at as u64 <= now {
        return InclusionDecision {
            included: false,
            reason: format!("expired address list expire_at={expire_at}"),
        };
    }

    if !config.allow_private_ips && !is_public_ipv4(ip) {
        return InclusionDecision {
            included: false,
            reason: format!("non-public IPv4 address {ip}"),
        };
    }

    if candidate.success_count < config.min_successes
        && !(config.include_seed_nodes && candidate.seed)
    {
        return InclusionDecision {
            included: false,
            reason: format!(
                "success_count {} is below min_successes {}",
                candidate.success_count, config.min_successes
            ),
        };
    }

    InclusionDecision {
        included: true,
        reason: "included".to_owned(),
    }
}

fn candidate_report(candidate: &Candidate, decision: InclusionDecision) -> ReportNode {
    let endpoint = first_udp4_endpoint(&candidate.node);
    ReportNode {
        key_id: candidate.key_id.to_string(),
        public_key: candidate.public_key.clone(),
        ip: endpoint.map(|(ip, _)| ip.to_string()),
        port: endpoint.map(|(_, port)| port),
        seed: candidate.seed,
        included: decision.included,
        reason: decision.reason,
        seen_count: candidate.seen_count,
        success_count: candidate.success_count,
        first_seen_at: candidate.first_seen_at,
        last_seen_at: candidate.last_seen_at,
        version: candidate.node.version,
        expire_at: candidate.node.addr_list.expire_at,
        last_error: candidate.last_error.clone(),
    }
}

fn dht_node_to_json(node: &DhtNodeConfig) -> Result<Value> {
    let addrs = node
        .addr_list
        .addrs
        .iter()
        .filter_map(address_to_json)
        .collect::<Vec<_>>();
    if addrs.is_empty() {
        bail!("DHT node has no serializable UDP addresses");
    }

    Ok(json!({
        "@type": "dht.node",
        "id": {
            "@type": "pub.ed25519",
            "key": node_public_key_base64(node)?,
        },
        "addr_list": {
            "@type": "adnl.addressList",
            "addrs": addrs,
            "version": node.addr_list.version,
            "reinit_date": node.addr_list.reinit_date,
            "priority": node.addr_list.priority,
            "expire_at": node.addr_list.expire_at,
        },
        "version": node.version,
        "signature": base64_encode(&node.signature),
    }))
}

fn address_to_json(address: &Address) -> Option<Value> {
    match address {
        Address::Adnl_Address_Udp(udp) => Some(json!({
            "@type": "adnl.address.udp",
            "ip": udp.ip,
            "port": udp.port,
        })),
        Address::Adnl_Address_Tunnel(_) | Address::Adnl_Address_Udp6(_) => None,
    }
}

fn set_output_dht_nodes(config: &mut Value, nodes: Vec<Value>) -> Result<()> {
    let dht = config
        .get_mut("dht")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow!("global config has no dht object"))?;
    dht.entry("@type")
        .or_insert_with(|| Value::String("dht.config.global".to_owned()));
    let static_nodes = dht
        .get_mut("static_nodes")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow!("global config has no dht.static_nodes object"))?;
    static_nodes
        .entry("@type")
        .or_insert_with(|| Value::String("dht.nodes".to_owned()));
    static_nodes.insert("nodes".to_owned(), Value::Array(nodes));
    Ok(())
}

fn first_udp4_endpoint(node: &DhtNodeConfig) -> Option<(Ipv4Addr, i32)> {
    node.addr_list
        .addrs
        .iter()
        .find_map(|address| match address {
            Address::Adnl_Address_Udp(udp) => Some((Ipv4Addr::from(udp.ip as u32), udp.port)),
            Address::Adnl_Address_Tunnel(_) | Address::Adnl_Address_Udp6(_) => None,
        })
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || octets[0] == 0
        || octets[0] >= 240)
}

fn node_key_id(node: &DhtNodeConfig) -> Result<Arc<KeyId>> {
    let key: Arc<dyn KeyOption> = (&node.id).try_into()?;
    Ok(key.id().clone())
}

fn node_public_key_base64(node: &DhtNodeConfig) -> Result<String> {
    match &node.id {
        PublicKey::Pub_Ed25519(key) => Ok(base64_encode(key.key.as_slice())),
        _ => bail!("DHT node has unsupported public key type"),
    }
}

fn read_json<T>(path: &Path) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    serde_json::from_reader(file).with_context(|| format!("failed to parse {}", path.display()))
}

fn read_json_value(path: &Path) -> Result<Value> {
    read_json(path)
}

fn write_json<T: Serialize>(path: &Path, value: &T, compact: bool) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let file =
        fs::File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    if compact {
        serde_json::to_writer(file, value)
    } else {
        serde_json::to_writer_pretty(file, value)
    }
    .with_context(|| format!("failed to write {}", path.display()))
}

fn write_json_value(path: &Path, value: &Value, compact: bool) -> Result<()> {
    write_json(path, value, compact)
}

fn config_base_dir(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

fn resolve_config_path(base_dir: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    }
}

fn default_local_adnl_addr() -> String {
    "0.0.0.0:4192".to_owned()
}

fn default_crawl_rounds() -> usize {
    6
}

fn default_peer_timeout_secs() -> u64 {
    8
}

fn default_max_known_nodes() -> usize {
    1000
}

fn default_max_output_nodes() -> usize {
    200
}

fn default_min_successes() -> u32 {
    1
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_ipv4_filter_rejects_non_public_ranges() {
        assert!(is_public_ipv4(Ipv4Addr::new(51, 195, 248, 121)));
        assert!(!is_public_ipv4(Ipv4Addr::new(10, 0, 0, 1)));
        assert!(!is_public_ipv4(Ipv4Addr::new(127, 0, 0, 1)));
        assert!(!is_public_ipv4(Ipv4Addr::new(192, 0, 2, 1)));
        assert!(!is_public_ipv4(Ipv4Addr::new(224, 0, 0, 1)));
    }

    #[test]
    fn set_output_dht_nodes_replaces_only_static_nodes() {
        let mut config = json!({
            "@type": "config.global",
            "dht": {
                "@type": "dht.config.global",
                "k": 6,
                "a": 3,
                "static_nodes": {
                    "@type": "dht.nodes",
                    "nodes": [{"old": true}]
                }
            },
            "validator": {"zero_state": {"file_hash": "keep"}}
        });

        set_output_dht_nodes(&mut config, vec![json!({"new": true})]).unwrap();

        assert_eq!(
            config["dht"]["static_nodes"]["nodes"],
            json!([{"new": true}])
        );
        assert_eq!(
            config["validator"]["zero_state"]["file_hash"],
            Value::String("keep".to_owned())
        );
    }
}
