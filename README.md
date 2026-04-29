# eh2telegraph

[中文](README-zh.md)|英文

This project is a heavily optimized and refactored fork of [qini7-sese/eh2telegraph:master](https://github.com/qini7-sese/eh2telegraph).

The original contributors of the upstream project are not associated with this fork, and hold no responsibility for the code quality, stability, security, or ongoing maintenance of this repository. For any issues, bug reports, or feature requests, please open an issue in this repository directly and refrain from contacting the original authors.

Bot that fetches image sets from EH/EX/NH and generates Telegraph pages.

After revoking `native-tls`, this fork aims to support macOS, Linux, and Windows.

**This project depends on a Cloudflare Worker proxy for image delivery.**

If the proxy is unavailable or blocked:
- Images in generated Telegraph pages may not load
- Existing pages may become partially broken

**For production use, it is strongly recommended to deploy your own Worker.**

## Performance Improvements

As a heavily optimized fork, this project significantly reduces memory footprint and CPU overhead compared to the upstream repository.

Below is a profiling comparison between the original upstream version (Before) and our refactored version (After):

![Performance](./assets/metrics.svg)

*Note: The performance gain mainly comes from TLS clients reusing (~90% tmp alloc), memory-intensive code minimising (~40% mem leak), and yet, the most CPU offload benefits attributed to eliminating image downloads and re-uploads, which changes the delivery model to proxy-based embedding.*

## Docker-free Deployment Guidelines

### Windows

1. Install the required tools:
   - Git
   - Visual Studio 2022 Build Tools with C++ build tools
   - Rust via rustup

2. Clone and enter the project:

```powershell
git clone https://github.com/DrCMWither/eh2telegraph.git
cd eh2telegraph
```


3. Create the config file:

```powershell
Copy-Item .\config_example.yaml .\config.yaml
notepad .\config.yaml
```

4. Edit `config.yaml`.

5. Build the bot:

```powershell
cargo build --release -p bot
```

6. Run the bot:

```powershell
.\target\release\bot.exe -c .\config.yaml
```

7. Optional: set a persistent Telegram proxy for unstable networks:

```powershell
[System.Environment]::SetEnvironmentVariable(
  "TELOXIDE_PROXY",
  "http://127.0.0.1:YourProxyPort",
  "User"
)
```

Restart PowerShell after setting it.

8. View logs directly in the running terminal.

9. To stop the bot, just simply use `Ctrl + C`.

### Linux

1. Install the required tools:

```bash
sudo apt update
sudo apt install -y git build-essential pkg-config libssl-dev curl
```

2. Install Rust:

```bash
curl https://sh.rustup.rs -sSf | sh
source "$HOME/.cargo/env"
```

3. Clone and enter the project:

```bash
git clone https://github.com/DrCMWither/eh2telegraph.git
cd eh2telegraph
```

4. Create the config file:

```bash
cp config_example.yaml config.yaml
nano config.yaml
```

5. Edit `config.yaml`.

6. Build the bot:

```bash
cargo build --release -p bot
```

7. Run the bot:

```bash
./target/release/bot -c ./config.yaml
```

8. Optional: set a Telegram proxy for the current shell:

```bash
export TELOXIDE_PROXY="http://127.0.0.1:YourProxyPort"
```

To make it persistent:

```bash
echo 'export TELOXIDE_PROXY="http://127.0.0.1:YourProxyPort"' >> ~/.bashrc
source ~/.bashrc
```

9. Optional: run it as a systemd service.

Create `/etc/systemd/system/eh2telegraph.service`:

```ini
[Unit]
Description=eh2telegraph bot
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=/opt/eh2telegraph
ExecStart=/opt/eh2telegraph/target/release/bot -c /opt/eh2telegraph/config.yaml
Restart=always
RestartSec=5
Environment=RUST_LOG=info
# Environment=TELOXIDE_PROXY=http://127.0.0.1:7890

[Install]
WantedBy=multi-user.target
```

Then enable and start:

```bash
sudo systemctl daemon-reload
sudo systemctl enable eh2telegraph
sudo systemctl start eh2telegraph
```

View logs:

```bash
journalctl -u eh2telegraph -f
```

Stop:

```bash
sudo systemctl stop eh2telegraph
```

Update:

```bash
cd /opt/eh2telegraph
git pull
cargo build --release -p bot
sudo systemctl restart eh2telegraph
```


## Deployment Guidelines via Docker

Docker deployment is still supported, but this fork primarily documents Docker-free deployment for easier manual operation and debugging.

1. Install Docker and docker-compose.

2. Create a new folder `ehbot`.

3. Copy `config_example.yaml` from the project to `ehbot` and rename it to `config.yaml`, then change the configuration details (see the next section).

4. Copy `docker-compose.yml` to `ehbot`.

5. Start and Shutdown.

    1. Start: Run `docker-compose up -d` in this folder.

    2. Shutdown: Run `docker-compose down` in this folder.

    3. View logs: Run `docker-compose logs` in this folder.

    4. Update the image: Run `docker-compose pull` in this folder.

## Configuration Guidelines

1. Basic Configuration

    1. Bot Token: Find @BotFather in Telegram to apply.

    2. Admin (can be empty): your Telegram ID, you can get it from any relevant Bot (you can also get it from this Bot `/id`).

    3. Telegraph: Use your browser to create a Telegraph Token via [this link](https://api.telegra.ph/createAccount?short_name=test_account&author_name=test_author) and fill in. You can also change the author name and URL.

2. Proxy Configuration

    1. Deploy `worker/web_proxy.js` of this repository to Cloudflare Workers and configure the `KEY` environment variable to be a random string (the purpose of the `KEY` is to prevent unauthorized requests to the proxy).

    2. Fill in the URL and Key into the yaml.

    3. The proxy is used to request some services with frequency limitation, so do not abuse it.

3. IPv6 configuration

    1. You can specify an IPv6 segment, if you do not have a larger (meaning larger than `/64`) IPv6 segment, please leave it blank.

    2. Configure IPv6 to somewhat alleviate the flow restriction for single IP.

4. Configure cookies for some Collectors.

    1. Currently, only exhentai is required.

5. KV configuration

    1. This project uses a built-in caching service to avoid repeated synchronization of an image set.

    2. Please refer to [cloudflare-kv-proxy](https://github.com/ihciah/cloudflare-kv-proxy) for deployment and fill in the yaml file.

    3. If you don't want to use remote caching, you can also use pure memory caching (it will be invalid after reboot). If you want to do so, you need to modify the code and recompile it by yourself.

## Development Guidelines

### Environment

This project is fully compatible with stable Rust (>=1.95), **no nightly features are required.** Recommended to use VSCode or Clion for development.

[RsProxy](https://rsproxy.cn/) is recommended as the crates.io source and toolchain installation source for users in China Mainland.

### Version Release

A Docker build can be triggered by typing a Tag starting with `v`. You can type the tag directly in git and push it up; however, it is easier to publish the release in GitHub and fill in the `v` prefix.

## Technical Details

Although this project is a simple crawler, there are still some considerations that need to be explained.

### GitHub Action Builds

GitHub Action can be used to automatically build Docker images, and this project supports automatic builds for the `x86_64` platform.

However, it can also build `arm64` versions, but it is not enabled because it uses qemu to emulate the arm environment on x86_64, so it is extremely slow (more than 1h for a single build).

### IPv6 Ghost Client (it's not a well-known name, just made up by myself)

Some sites have IP-specific access frequency limits, which can be mitigated by using multiple IPs. The most common approach in practice is proxy pooling, but proxy pools are often extremely unstable and require maintenance and possibly some cost.

Observe the target sites of this project, many use Cloudflare, and Cloudflare supports IPv6 and the granularity of flow limitation is `/64`. If we bind a larger IPv6 segment for the local machine and randomly select IPs from it as client exit addresses, we can make more frequent requests steadily.

Since the NIC will only bind a single IPv6 address, we need to enable `net.ipv6.ip_nonlocal_bind`.

After configuring IPv6, for target sites that can use IPv6, this project will use random IP requests from the IPv6 segment.

Configuration (configuration for the NIC can be written in `if-up` for persistence).

1. `sudo ip add add local 2001:x:x::/48 dev lo`

2. `sudo ip route add local 2001:x:x::/48 dev your-interface`

3. Configure `net.ipv6.ip_nonlocal_bind=1` in Sysctl. This step varies by distribution (for example, the common `/etc/sysctl.conf` does not exist in Arch Linux).

Where to get IPv6? he.net offers a free service for this, but of course it is not expensive to buy an IPv6 IP segment yourself.

You can test the configuration with `curl --interface 2001:***** ifconfig.co` to see if it is correct.

### Forcing IPv6

The site mentioned in the previous subsection uses Cloudflare, but in fact does not really enable IPv6. when you specify the ipv6 request directly using curl, you will find that it has no AAAA records at all. But because the CF infrastructure is Anycast, so if the target site does not explicitly deny IPv6 visitors in the code, they can still be accessed through IPv6.

1. telegra.ph: No AAAA records, but force resolves to Telegram's entry IP for access, but the certificate is `*.telegram.org`.

    ~~This project writes a TLS validator that checks the validity of a given domain's certificate, to allow for misconfiguration of its certificate while maintaining security.~~

    However, Telegraph fixed the problem very quickly, so the TLS verifier is currently disabled.

2. EH/NH: Forced IPv6 availability.

3. EX: CF is not used and no IPv6 service is available.

### Proxy

This project uses Cloudflare Workers as a partial API proxy to alleviate the flow limitation problem when IPv6 is not available. See `src/http_proxy.rs` and `worker/web_proxy.js`.

### Caching

To minimize duplicate pulls, this project uses in-memory caching and remote persistent caching. Remote persistent cache using Cloudflare Worker with Cloudflare KV to build. The main project code reference is [cloudflare-kv-proxy](https://github.com/ihciah/cloudflare-kv-proxy).

Since it takes some time to synchronize image sets, to avoid repeated synchronization, this project uses [singleflight-async](https://github.com/ihciah/singleflight-async) to reduce this kind of waste.

### Image Embedding Proxy?

Since telegra.ph removed `upload` API for image due to the excessive spam and abuse in 2024, the original repo is no longer reliable. Instead, this fork employs an image embedding proxy strategy to make it still available. Currently, the image embedding proxy code is coupled with the legacy `worker/web_proxy.js`.

### Benchmark Methodology

- Dataset: 10 batches, 3 EH/NH galleries(~100 pages total) for each batches
- Environment: AMD Ryzen 9 7940H / 96G RAM / Ubuntu
- Tooling: heaptrack / perf / samply

*Benchmark scripts are not included yet, results are indicative.*

## Contribute Guidelines

You are welcome to contribute code to this project (no matter how small the commit is)!
