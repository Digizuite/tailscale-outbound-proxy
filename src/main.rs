mod finalizers;
mod replace_service_reconciler;
mod replaced_service;
mod tailscale_api;

extern crate pretty_env_logger;
#[macro_use]
extern crate log;

use crate::replace_service_reconciler::reconcile_replaced_service;
use crate::replaced_service::ReplacedService;
use crate::tailscale_api::{new_tailscale_api, TailscaleApi};
use clap::Parser;
use futures::stream::StreamExt;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Service;
use kube::client::Client;
use kube::{Api, CustomResourceExt, Resource};
use kube_runtime::controller::Action;
use kube_runtime::watcher::Config;
use kube_runtime::Controller;
use serde::de::DeserializeOwned;
use std::fmt::Debug;
use std::fs::File;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(long_about = None)]
struct Args {
    /// If the crd definitions should be written out
    #[arg(long, env = "GENERATE_CRDS")]
    generate_crds: bool,

    /// The client id of the tailscale OAuth client
    #[arg(long, env = "TAILSCALE_CLIENT_ID")]
    tailscale_client_id: String,

    /// The client secret of the Tailscale OAuth client
    #[arg(long, env = "TAILSCALE_CLIENT_SECRET")]
    tailscale_client_secret: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    pretty_env_logger::init_timed();

    info!("Build Timestamp: {}", env!("VERGEN_BUILD_TIMESTAMP"));
    info!("git hash: {}", env!("VERGEN_GIT_DESCRIBE"));
    
    let args = Args::parse();

    if args.generate_crds {
        write_crds()?;
    }

    let tailscale_api = new_tailscale_api(
        args.tailscale_client_id.clone(),
        args.tailscale_client_secret.clone(),
    )
    .await?;

    info!("Starting tailscale outbound proxy operator");

    let kubernetes_client = Client::try_default().await?;

    let replaced_services_api: Api<ReplacedService> = Api::all(kubernetes_client.clone());
    let deployments_api: Api<Deployment> = Api::all(kubernetes_client.clone());
    let services_api: Api<Service> = Api::all(kubernetes_client.clone());

    let context = Arc::new(ContextData {
        kubernetes_client: kubernetes_client.clone(),
        http_client: reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?,
        danger_ignore_certs_http_client: reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(10))
            .build()?,
        tailscale_api_client: tailscale_api,
    });

    Controller::new(replaced_services_api.clone(), Config::default())
        .owns(deployments_api, Config::default())
        .owns(services_api, Config::default())
        .run(reconcile_replaced_service, error_policy, context)
        .for_each(|res| async move {
            match res {
                Ok(o) => debug!("reconciled: {:?}", o),
                Err(e) => error!("reconcile failed: {:?}", e),
            }
        })
        .await;

    Ok(())
}

struct ContextData {
    kubernetes_client: Client,
    http_client: reqwest::Client,
    danger_ignore_certs_http_client: reqwest::Client,
    tailscale_api_client: TailscaleApi,
}

fn write_crds() -> anyhow::Result<()> {
    let file_path = "charts/tailscale-outbound-proxy/templates/crds.yaml";

    let mut file = File::create(file_path)?;

    write_crd::<ReplacedService>(&mut file)?;

    Ok(())
}

fn write_crd<TResource: CustomResourceExt>(mut file: &mut File) -> anyhow::Result<()> {
    let crd = TResource::crd();

    serde_yaml::to_writer(&mut file, &crd)?;
    write!(file, "\n---\n")?;

    Ok(())
}

/// All errors possible to occur during reconciliation
#[derive(thiserror::Error, Debug)]
#[error(transparent)]
pub struct Error(#[from] anyhow::Error);

fn error_policy<TResource>(
    echo: Arc<TResource>,
    error: &Error,
    _context: Arc<ContextData>,
) -> Action
where
    TResource:
        Clone + Resource + CustomResourceExt + DeserializeOwned + Debug + Send + Sync + 'static,
{
    error!(
        "Reconciliation error while reconciling type {}:\n{:?}.\n{:?}",
        TResource::crd_name(),
        error,
        echo
    );
    Action::requeue(Duration::from_secs(15))
}
