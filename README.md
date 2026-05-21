# Everscale Global Config Builder

Builds an Everscale `global-config.json` from verified DHT bootstrap peers.

This project does not turn validator IP addresses into global-config entries. A global config must contain signed `dht.node` records. The builder starts from a seed config, crawls Everscale DHT, accepts only signed DHT nodes that pass library verification, checks reachability, and writes a new config plus a report.

## Why This Exists

The old public Everscale global config can become stale. A private seed config can work, but publishing a config made only from one operator's nodes is not ideal. This builder makes the process repeatable:

- start from a small seed config;
- discover signed DHT nodes from the live Everscale DHT;
- verify signatures through `ever-adnl`;
- keep only reachable public IPv4 peers by default;
- preserve all non-DHT fields from the seed config;
- replace `dht.static_nodes.nodes` with the verified set.

If a validator is reachable by ADNL but does not expose a signed DHT node, it cannot safely be inserted into `dht.static_nodes.nodes`.

## Usage

Install on a server:

```bash
git clone https://github.com/jouliene/everscale_global_config_builder.git
cd everscale_global_config_builder
./install.sh
```

Create a config from the example:

```bash
cp everscale_global_config_builder.example.json everscale_global_config_builder.json
```

Edit `seed_global_config_path`, then run:

```bash
cargo run --release -- build --config everscale_global_config_builder.json
```

After `./install.sh`, you can run the release binary directly:

```bash
target/release/everscale_global_config_builder build --config everscale_global_config_builder.json
```

Check the result:

```bash
jq '.summary' out/everscale-global-config-report.json
jq '.dht.static_nodes.nodes | length' out/everscale-global-config.json
```

Use a local ADNL port that is not already used by the resolver service. The example uses `0.0.0.0:4192` so it does not conflict with a resolver on `4191`.

## Output Policy

Defaults are conservative:

- `include_seed_nodes: false` means seed nodes are not forced into the output unless they are observed as reachable during this run.
- `allow_private_ips: false` excludes private, loopback, multicast, link-local, documentation, and unspecified IPv4 addresses.
- `min_successes: 1` requires at least one successful live query or ping before a node is emitted.

For a public release, run the builder more than once and preferably from more than one network location. A good config should not depend on a single server, provider, or geography.
