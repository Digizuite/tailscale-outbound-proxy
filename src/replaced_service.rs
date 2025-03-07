use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Replaces this specified service with a service that is proxied through tailscale
#[derive(CustomResource, Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
#[kube(
    group = "tsproxy.digizuite.com",
    version = "v1alpha1",
    kind = "ReplacedService",
    plural = "replacedservices",
    derive = "PartialEq",
    status = "ReplacedServiceResourceStatus",
    printcolumn = r#"{"name":"Tailscale Hostname", "type":"string", "description":"Hostname of the tailscale machine to ssh to", "jsonPath":".status.tailscaleHostname"}"#,
    printcolumn = r#"{"name":"Ready", "type":"boolean", "description":"If the proxy is ready and handling requests", "jsonPath":".status.active"}"#,
    namespaced
)]
#[serde(rename_all = "camelCase")]
pub struct ReplacedServiceSpec {
    /// The name of the kubernetes service to replace
    pub service_to_replace: String,

    /// How to map the original ports through the proxy
    pub ports: Vec<ServicePortMapping>,

    /// Node selectors to the apply to the deployment that replaces the service
    pub node_selector: Option<BTreeMap<String, String>>,

    /// A path to a health endpoint to check if the service is up
    pub health_endpoint: Option<String>,

    /// What protocol to use when testing the endpoint
    pub health_test_protocol: Option<TestProtocol>,

    /// If certificate errors should be ignored when testing a https endpoint
    pub ignore_certificate_errors: Option<bool>,

    /// The name the tailscale proxy should have in the tailnet
    pub tailscale_host_name: String,

    /// The tags the tailscale proxy should have in the tailnet
    pub tailscale_tags: Vec<String>,

    /// The service account the proxy pod should use.
    pub service_account: String,

    /// The secret to use for storing tailscales state. You do not have
    /// to create this secret yourself.
    pub proxy_state_secret_name: String,

    /// The name of a keda scaled object that might prevent downscaling of a deployment
    pub keda_scale_object_name: Option<String>,

    /// A custom image to use for the tailscale proxy
    pub replaced_service_tailscale_image: Option<String>,

    /// A pull secret to use for the custom image. Do note proxies are started in the same
    /// namespace as this CRD instance, meaning the pull secret should be in the same
    /// namespace as this CRD instance.
    pub replaced_service_tailscale_image_pull_secret: Option<String>
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema, Default)]
pub enum TestProtocol {
    #[default]
    Http,
    Https,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServicePortMapping {
    /// If the service that is being replaced has multiple ports, then
    /// this is used to map between the ports. Not needed if there is only
    /// 1 port on the replaced service.
    pub original_port: Option<i32>,
    /// The port that should be invoked on the proxy
    pub proxy_port: i32,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ReplacedServiceResourceStatus {
    /// The hostname of the tailscale machine to ssh to
    pub tailscale_hostname: Option<String>,
    /// The name of the deployment that was scaled down to replace the service
    pub replaced_deployment: Option<String>,
    /// Warnings that occured during last reconcile.
    pub warning: Option<String>,

    /// If this is actually working yet.
    pub active: Option<bool>,
}