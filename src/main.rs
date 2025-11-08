use std::collections::HashMap;

use bollard::Docker;
use bollard::query_parameters::{EventsOptionsBuilder, ListContainersOptionsBuilder};
use bollard::secret::EventMessage;
use cloudflare::endpoints::dns::dns::{CreateDnsRecord, CreateDnsRecordParams, DnsContent};
use cloudflare::endpoints::zones::zone::{ListZones, ListZonesParams};
use cloudflare::framework::Environment;
use cloudflare::framework::auth::Credentials;
use cloudflare::framework::client::ClientConfig;
use cloudflare::framework::client::async_api::Client;
use eyre::bail;
use futures::StreamExt;
use serde::Deserialize;
use tokio::fs;
use tokio::process::Command;
use tracing::level_filters::LevelFilter;
use tracing::{error, info, trace, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Registry;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_tree::HierarchicalLayer;

#[derive(Deserialize)]
struct Config {
    target: String,
    cloudflare: CloudflareConfig,
    ssh_dnsmasq: SshDnsmasqConfig,
}

#[derive(Deserialize)]
struct CloudflareConfig {
    token: String,
}

#[derive(Deserialize)]
struct SshDnsmasqConfig {
    host: String,
    user: String,
    key_file: String,
    config_dir: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> eyre::Result<()> {
    color_eyre::install()?;

    let config: Config = toml::from_slice(&fs::read("config.toml").await?)?;

    Registry::default()
        .with(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .with(HierarchicalLayer::new(2).with_indent_lines(true))
        .init();

    let cloudflare = Client::new(
        Credentials::UserAuthToken {
            token: config.cloudflare.token.clone(),
        },
        ClientConfig::default(),
        Environment::Production,
    )?;

    let docker = Docker::connect_with_defaults()?;

    let containers = docker
        .list_containers(Some(ListContainersOptionsBuilder::new().all(true).build()))
        .await?;

    for container in containers {
        let name = container.names.as_ref().and_then(|names| names.first());

        let Some(name) = name else { continue };

        let hostname = container
            .labels
            .as_ref()
            .and_then(|labels| labels.get("dev.hasali.docker-ddns.hostname"));

        let Some(hostname) = hostname else { continue };

        info!(name, hostname, "Checking records for existing container");

        create_dns_records(&cloudflare, &config.ssh_dnsmasq, hostname, &config.target).await?;
    }

    info!("Listening for docker events...");

    while let Some(event) = docker
        .events(Some(
            EventsOptionsBuilder::new()
                .filters(&HashMap::from_iter([
                    ("type", vec!["container"]),
                    ("event", vec!["create", "destroy"]),
                ]))
                .build(),
        ))
        .next()
        .await
    {
        let event = match event {
            Ok(event) => event,
            Err(e) => {
                error!("Failed to read event: {e}");
                continue;
            }
        };

        match event.action.as_deref() {
            Some("create") => {
                if let Err(e) =
                    handle_new_container(&cloudflare, &config.ssh_dnsmasq, event, &config.target)
                        .await
                {
                    error!("Error handling new container event: {e}");
                    eprintln!("{e:?}");
                    continue;
                }
            }
            Some("destroy") => {
                // TODO: Remove DNS records
            }
            _ => continue,
        }
    }

    Ok(())
}

async fn handle_new_container(
    cloudflare: &Client,
    dnsmasq: &SshDnsmasqConfig,
    event: EventMessage,
    target: &str,
) -> eyre::Result<()> {
    let Some(actor) = event.actor else {
        return Ok(());
    };

    let Some(attributes) = actor.attributes else {
        return Ok(());
    };

    let Some(name) = attributes.get("name") else {
        return Ok(());
    };

    let Some(hostname) = attributes.get("dev.hasali.docker-ddns.hostname") else {
        return Ok(());
    };

    info!(name, hostname, "Detected new container");

    if let Err(e) = create_dns_records(cloudflare, dnsmasq, hostname, target).await {
        error!("Failed to create dns records for '{hostname}': {e}");
        eprintln!("{e:?}");
    }

    Ok(())
}

#[tracing::instrument(skip(cloudflare, dnsmasq))]
async fn create_dns_records(
    cloudflare: &Client,
    dnsmasq: &SshDnsmasqConfig,
    hostname: &str,
    target: &str,
) -> eyre::Result<()> {
    info!("Creating dns records");

    create_cloudflare_record(cloudflare, hostname, target).await;

    create_dnsmasq_record(dnsmasq, hostname, target).await?;

    Ok(())
}

#[tracing::instrument(skip(client))]
async fn create_cloudflare_record(client: &Client, hostname: &str, target: &str) {
    let zones = client
        .request(&ListZones {
            params: ListZonesParams::default(),
        })
        .await;

    let Ok(zones) = zones else {
        error!("Failed to list zones");
        return;
    };

    let zone = zones
        .result
        .iter()
        .find(|zone| hostname.ends_with(&zone.name));

    let Some(zone) = zone else {
        warn!("No zone found matching hostname");
        return;
    };

    info!("Attempting to create DNS record");

    let res = client
        .request(&CreateDnsRecord {
            zone_identifier: &zone.id,
            params: CreateDnsRecordParams {
                name: hostname,
                content: DnsContent::CNAME {
                    content: target.to_owned(),
                },
                proxied: Some(false),
                priority: None,
                ttl: None,
            },
        })
        .await;

    if res.is_err() {
        warn!("Failed to create DNS record, likely already exists");
        return;
    }

    info!("DNS record created successfully");
}

#[tracing::instrument(skip(config))]
async fn create_dnsmasq_record(
    config: &SshDnsmasqConfig,
    hostname: &str,
    target: &str,
) -> eyre::Result<()> {
    let config_dir = &config.config_dir;

    #[rustfmt::skip]
    let remote_cmd = format!(r#"
        ([ -e {config_dir}/{hostname} ] && echo 'Already exists') || (
            echo 'cname={hostname},{target}' > {config_dir}/{hostname} &&
            echo "Created config entry"
        )
    "#);

    let output = Command::new("ssh")
        .args(["-i", &config.key_file])
        .args(["-o", "StrictHostKeyChecking=accept-new"])
        .arg(format!("{}@{}", config.user, config.host))
        .arg(remote_cmd)
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stdout = stdout.trim();
    if !stdout.is_empty() {
        trace!("stdout: {stdout}");
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        trace!("stderr: {stderr}");
    }

    if stdout.contains("Created config entry") {
        info!("Created dns record");
    } else if stdout.contains("Already exists") {
        info!("Record already exists");
    } else {
        warn!("Unrecognised output");
    }

    if !output.status.success() {
        bail!(
            "Failed to update dnsmasq config: ssh process exited with code {:?}",
            output.status.code()
        );
    }

    Ok(())
}
