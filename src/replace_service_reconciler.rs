use crate::finalizers::{ensure_finalizer, remove_finalizer};
use crate::replaced_service::{ReplacedService, ReplacedServiceResourceStatus, TestProtocol};
use crate::{ContextData, Error};
use anyhow::{anyhow, Result};
use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy, ReplicaSet};
use k8s_openapi::api::core::v1::{
    Capabilities, Container, Endpoints, EnvVar, EnvVarSource, Pod, PodSpec, PodTemplateSpec,
    Secret, SecretKeySelector, SecurityContext, Service, ServicePort, ServiceSpec,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use k8s_openapi::NamespaceResourceScope;
use kube::api::{Patch, PatchParams};
use kube::core::object::HasStatus;
use kube::{Api, Client, Resource, ResourceExt};
use kube_runtime::controller::Action;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;

pub(crate) async fn reconcile_replaced_service(
    resource: Arc<ReplacedService>,
    context: Arc<ContextData>,
) -> Result<Action, Error> {
    match run_reconciliation(resource.clone(), context.clone()).await {
        Ok((action, ready)) => {
            set_active_state(resource, context, ready).await?;
            Ok(action)
        }
        Err(e) => {
            set_active_state(resource, context, false).await?;
            Err(e.into())
        }
    }
}

async fn set_active_state(
    resource: Arc<ReplacedService>,
    context: Arc<ContextData>,
    state: bool,
) -> Result<()> {
    if let Some(ns) = resource.namespace() {
        let api: Api<ReplacedService> = Api::namespaced(context.kubernetes_client.clone(), &ns);

        if let Some(mut latest) = api.get_opt(&resource.name_any()).await? {
            let status = latest.status_mut();
            if let Some(status) = status {
                status.active = Some(state);
            } else {
                *status = Some(ReplacedServiceResourceStatus {
                    active: Some(state),
                    ..Default::default()
                });
            }

            let patch = Patch::Merge(&latest);
            api.patch_status(&resource.name_any(), &PatchParams::default(), &patch)
                .await?;
        }
    }

    Ok(())
}

async fn run_reconciliation(
    resource: Arc<ReplacedService>,
    context: Arc<ContextData>,
) -> Result<(Action, bool)> {
    let name = resource.name_any();

    let namespace = resource.namespace().ok_or(anyhow!(
        "Expected SftpgoUser resource to be namespaced. Can't deploy to unknown namespace."
    ))?;

    let resource_api: Api<ReplacedService> =
        Api::namespaced(context.kubernetes_client.clone(), &namespace);

    let resource = resource_api.get_opt(&name).await?;

    if resource.is_none() {
        return Ok((Action::await_change(), false));
    }

    let resource = resource.unwrap();

    let service_to_replace_name = resource.spec.service_to_replace.clone();
    let test_proxy_service_name = format!("{}-proxy", service_to_replace_name);
    let backup_service_name = format!("{}-proxy-backup", service_to_replace_name);

    if resource.metadata.deletion_timestamp.is_some() {
        info!("Deleting resource {}", resource.name_any());

        let services_api = Api::namespaced(context.kubernetes_client.clone(), &namespace);
        if let Some(backup_service) = services_api.get_opt(&backup_service_name).await? {
            create_copy_of_original_service(
                &service_to_replace_name,
                &backup_service,
                context.kubernetes_client.clone(),
            )
            .await?;

            remove_finalizer::<Service>(
                context.kubernetes_client.clone(),
                &backup_service_name,
                &namespace,
            )
            .await?;
            services_api
                .delete(&backup_service_name, &Default::default())
                .await?;
        }

        if let Some(deployment_name) = resource
            .status
            .as_ref()
            .and_then(|s| s.replaced_deployment.as_ref())
        {
            change_deployment_scale(
                context.kubernetes_client.clone(),
                &namespace,
                deployment_name,
                1,
            )
            .await?;
        }

        remove_finalizer::<ReplacedService>(context.kubernetes_client.clone(), &name, &namespace)
            .await?;

        return Ok((Action::await_change(), false));
    }

    let resource = ensure_finalizer(resource, context.kubernetes_client.clone()).await?;
    let uid = resource
        .uid()
        .ok_or_else(|| anyhow!("Resource does not have a uid assigned"))?;

    let deployment_name = match find_deployment(&context, &resource).await? {
        Either::Left(deployment_name) => deployment_name,
        Either::Right(action) => return Ok(action),
    };

    let tsproxy_labels =
        BTreeMap::from([("app".to_string(), "tailscale-outbound-proxy".to_string())]);

    let mut owner_reference = resource.controller_owner_ref(&()).unwrap();
    owner_reference.controller = Some(false);

    ensure_tailscale_proxy(&context, &resource, &tsproxy_labels, &owner_reference).await?;

    let services_api: Api<Service> = Api::namespaced(context.kubernetes_client.clone(), &namespace);

    if let Some(service) = services_api.get_opt(&service_to_replace_name).await? {
        let is_replaced_service = service.owner_references().iter().any(|r| r.uid == uid);

        if is_replaced_service {
            info!(
                "Service {} is currently replaced service",
                service_to_replace_name
            );

            match test_if_proxy_service_works(&resource, context.clone(), &test_proxy_service_name)
                .await
            {
                Ok(_) => {
                    info!("Proxy service still works, nothing to do");
                }
                Err(err) => {
                    info!(
                        "Proxy service does not work, returning original service pointer: {}",
                        err
                    );

                    change_deployment_scale(
                        context.kubernetes_client.clone(),
                        &namespace,
                        &deployment_name,
                        1,
                    )
                    .await?;

                    restore_proxied_service(
                        &service_to_replace_name,
                        &namespace,
                        context.kubernetes_client.clone(),
                    )
                    .await?;

                    let warning = format!(
                        "Proxy service does not work, returning original service pointer: {err}"
                    );
                    return add_warning_and_requeue(resource, resource_api, &warning).await;
                }
            };
        } else {
            create_copy_of_original_service(
                &backup_service_name,
                &service,
                context.kubernetes_client.clone(),
            )
            .await?;

            let service_ports: Vec<i32> = service
                .spec
                .as_ref()
                .and_then(|spec| spec.ports.as_ref())
                .map(|ports| ports.iter().map(|p| p.port).collect())
                .unwrap_or_default();

            if service_ports.is_empty() {
                return add_warning_and_requeue(
                    resource,
                    resource_api,
                    "Service has no ports defined",
                )
                .await;
            }

            let port_map = match generate_port_map(
                service_ports,
                resource.clone(),
                resource_api.clone(),
            )
            .await
            {
                Ok(value) => value,
                Err(value) => return value,
            };

            info!("Port map: {:?}", port_map);

            let test_proxy_service = Service {
                metadata: ObjectMeta {
                    name: Some(test_proxy_service_name.clone()),
                    namespace: Some(namespace.clone()),
                    owner_references: Some(vec![owner_reference.clone()]),
                    ..Default::default()
                },
                spec: Some(ServiceSpec {
                    ports: Some(
                        port_map
                            .iter()
                            .map(|(service_port, proxy_port)| ServicePort {
                                port: *service_port,
                                target_port: Some(IntOrString::Int(*proxy_port)),
                                ..Default::default()
                            })
                            .collect(),
                    ),
                    selector: Some(tsproxy_labels.clone()),
                    ..Default::default()
                }),
                ..Default::default()
            };

            let test_proxy_service =
                do_server_side_apply(context.kubernetes_client.clone(), test_proxy_service).await?;

            match test_if_proxy_service_works(&resource, context.clone(), &test_proxy_service_name)
                .await
            {
                Ok(_) => {
                    info!("Proxy service works, updating service pointer");
                }
                Err(err) => {
                    info!("Proxy service does not work, leaving service pointer as is");
                    let warning = format!(
                        "Proxy service does not work, leaving service pointer as is: {err}"
                    );
                    return add_warning_and_requeue(resource, resource_api, &warning).await;
                }
            }

            let proper_proxy_service = Service {
                metadata: ObjectMeta {
                    name: Some(service_to_replace_name.clone()),
                    namespace: Some(namespace.clone()),
                    owner_references: Some(vec![owner_reference.clone()]),
                    ..Default::default()
                },
                spec: Some(ServiceSpec {
                    ports: test_proxy_service.spec.and_then(|s| s.ports).clone(),
                    selector: Some(tsproxy_labels.clone()),
                    ..Default::default()
                }),
                ..Default::default()
            };

            do_server_side_apply(context.kubernetes_client.clone(), proper_proxy_service).await?;

            change_deployment_scale(
                context.kubernetes_client.clone(),
                &namespace,
                &deployment_name,
                0,
            )
            .await?;
        }
    } else {
        let warning = format!("Service {} was not found", service_to_replace_name);
        return add_warning_and_requeue(resource, resource_api, &warning).await;
    }

    remove_warning_and_requeue(resource, resource_api).await
}

async fn ensure_tailscale_proxy(
    context: &Arc<ContextData>,
    resource: &ReplacedService,
    tsproxy_labels: &BTreeMap<String, String>,
    owner_reference: &OwnerReference,
) -> Result<()> {
    info!("Ensuring tailscale proxy");
    let namespace = resource.namespace().unwrap();

    let tailscale_proxy_secret_state_secret_name = &resource.spec.proxy_state_secret_name;

    {
        let secret_api: Api<Secret> =
            Api::namespaced(context.kubernetes_client.clone(), &namespace);
        if secret_api
            .get_opt(tailscale_proxy_secret_state_secret_name)
            .await?
            .is_none()
        {
            let tags = resource.spec.tailscale_tags.clone();
            let auth_key = context.tailscale_api_client.create_auth_token(tags).await?;

            let secret = Secret {
                metadata: ObjectMeta {
                    name: Some(tailscale_proxy_secret_state_secret_name.to_string()),
                    namespace: Some(namespace.clone()),
                    owner_references: Some(vec![owner_reference.clone()]),
                    ..Default::default()
                },
                string_data: Some(BTreeMap::from([("TS_AUTHKEY".to_string(), auth_key)])),
                ..Default::default()
            };

            do_server_side_apply(context.kubernetes_client.clone(), secret).await?;
        }
    }

    create_resource_with_owner_reference(
        context.kubernetes_client.clone(),
        &namespace,
        "tailscale-outbound-proxy",
        owner_reference.clone(),
        || Deployment {
            metadata: ObjectMeta {
                labels: Some(tsproxy_labels.clone()),
                ..Default::default()
            },
            spec: Some(DeploymentSpec {
                replicas: Some(1),
                strategy: Some(DeploymentStrategy {
                    type_: Some("Recreate".to_string()),
                    ..Default::default()
                }),
                selector: LabelSelector {
                    match_labels: Some(tsproxy_labels.clone()),
                    ..Default::default()
                },
                template: PodTemplateSpec {
                    metadata: Some(ObjectMeta {
                        labels: Some(tsproxy_labels.clone()),
                        ..Default::default()
                    }),
                    spec: Some(PodSpec {
                        service_account: Some(resource.spec.service_account.clone()),
                        node_selector: resource.spec.node_selector.clone(),
                        init_containers: Some(vec![Container {
                            name: "sysctler".to_string(),
                            image: Some("busybox".to_string()),
                            command: Some(vec!["/bin/sh".to_string()]),
                            image_pull_policy: Some("Always".to_string()),
                            security_context: Some(SecurityContext {
                                privileged: Some(true),
                                ..Default::default()
                            }),
                            args: Some(vec![
                                "-c".to_string(),
                                "sysctl -w net.ipv4.ip_forward=1 net.ipv6.conf.all.forwarding=1"
                                    .to_string(),
                            ]),
                            ..Default::default()
                        }]),
                        containers: vec![Container {
                            name: "tailscale".to_string(),
                            image: Some("ghcr.io/digizuite/tailscale-fix:sha-a82e762".to_string()),
                            image_pull_policy: Some("Always".to_string()),
                            security_context: Some(SecurityContext {
                                capabilities: Some(Capabilities {
                                    add: Some(vec!["NET_ADMIN".to_string()]),
                                    ..Default::default()
                                }),
                                ..Default::default()
                            }),
                            env: Some(vec![
                                EnvVar {
                                    name: "TS_KUBE_SECRET".to_string(),
                                    value: Some(
                                        tailscale_proxy_secret_state_secret_name.to_string(),
                                    ),
                                    ..Default::default()
                                },
                                EnvVar {
                                    name: "TS_USERSPACE".to_string(),
                                    value: Some("true".to_string()),
                                    ..Default::default()
                                },
                                EnvVar {
                                    name: "TS_AUTH_ONCE".to_string(),
                                    value: Some("true".to_string()),
                                    ..Default::default()
                                },
                                EnvVar {
                                    name: "TS_EXTRA_ARGS".to_string(),
                                    value: Some("--ssh".to_string()),
                                    ..Default::default()
                                },
                                EnvVar {
                                    name: "TS_HOSTNAME".to_string(),
                                    value: Some(resource.spec.tailscale_host_name.clone()),
                                    ..Default::default()
                                },
                                EnvVar {
                                    name: "TS_AUTHKEY".to_string(),
                                    value_from: Some(EnvVarSource {
                                        secret_key_ref: Some(SecretKeySelector {
                                            name: Some(
                                                tailscale_proxy_secret_state_secret_name
                                                    .to_string(),
                                            ),
                                            optional: Some(false),
                                            key: "TS_AUTHKEY".to_string(),
                                        }),
                                        ..Default::default()
                                    }),
                                    ..Default::default()
                                },
                            ]),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }),
                },
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await?;
    Ok(())
}

enum Either<Left, Right> {
    Left(Left),
    Right(Right),
}

async fn find_deployment(
    context: &Arc<ContextData>,
    resource: &ReplacedService,
) -> Result<Either<String, (Action, bool)>> {
    let resource = resource.clone();
    let namespace = resource.namespace().unwrap();

    let resource_api: Api<ReplacedService> =
        Api::namespaced(context.kubernetes_client.clone(), &namespace);

    let service_to_replace_name = resource.spec.service_to_replace.clone();

    let deployment_name = if let Some(name) = resource
        .status
        .as_ref()
        .and_then(|s| s.replaced_deployment.as_ref())
    {
        name.clone()
    } else {
        let endpoint_api: Api<Endpoints> =
            Api::namespaced(context.kubernetes_client.clone(), &namespace);

        let endpoint = endpoint_api.get(&service_to_replace_name).await?;

        let target_ref = endpoint
            .subsets
            .as_ref()
            .and_then(|s| s.first())
            .and_then(|s| s.addresses.as_ref())
            .and_then(|a| a.first())
            .and_then(|a| a.target_ref.as_ref());
        if let Some(target_ref) = target_ref {
            if target_ref.kind == Some("Pod".to_string()) {
                let pod: Pod = Api::namespaced(context.kubernetes_client.clone(), &namespace)
                    .get(target_ref.name.as_ref().unwrap())
                    .await?;

                if let Some(pod_owner) = pod.owner_references().first() {
                    let replica_set: ReplicaSet =
                        Api::namespaced(context.kubernetes_client.clone(), &namespace)
                            .get(&pod_owner.name)
                            .await?;

                    if let Some(replica_set_owner) = replica_set.owner_references().first() {
                        let mut resource = resource.clone();
                        let deployment: Deployment =
                            Api::namespaced(context.kubernetes_client.clone(), &namespace)
                                .get(&replica_set_owner.name)
                                .await?;

                        let deployment_name = deployment.metadata.name.unwrap();
                        {
                            let status = resource.status_mut();

                            if let Some(s) = status {
                                s.replaced_deployment = Some(deployment_name.clone());
                            } else {
                                *status = Some(ReplacedServiceResourceStatus {
                                    replaced_deployment: Some(deployment_name.clone()),
                                    ..Default::default()
                                });
                            }
                        }

                        let resource_name = resource.name_any().clone();
                        resource_api
                            .patch_status(
                                &resource_name,
                                &Default::default(),
                                &Patch::Merge(resource),
                            )
                            .await?;

                        deployment_name
                    } else {
                        let warning = format!(
                            "Replica set {} does not have an owner reference",
                            replica_set.metadata.name.as_ref().unwrap()
                        );
                        return Ok(Either::Right(
                            add_warning_and_requeue(resource, resource_api, &warning).await?,
                        ));
                    }
                } else {
                    let warning = format!(
                        "Pod {} does not have an owner reference",
                        target_ref.name.as_ref().unwrap()
                    );
                    return Ok(Either::Right(
                        add_warning_and_requeue(resource, resource_api, &warning).await?,
                    ));
                }
            } else {
                let warning = format!(
                    "Service does not point to a pod. Got {} instead",
                    target_ref.kind.clone().unwrap_or_default()
                );
                return Ok(Either::Right(
                    add_warning_and_requeue(resource, resource_api, &warning).await?,
                ));
            }
        } else {
            return Ok(Either::Right(
                add_warning_and_requeue(resource, resource_api, "Service has no endpoints defined")
                    .await?,
            ));
        }
    };

    Ok(Either::Left(deployment_name))
}

async fn test_if_proxy_service_works(
    resource: &ReplacedService,
    context: Arc<ContextData>,
    test_proxy_service_name: &str,
) -> Result<()> {
    info!("Testing is proxy service for {test_proxy_service_name} works");
    let namespace = resource.namespace().unwrap();

    let proto = match resource
        .spec
        .health_test_protocol
        .clone()
        .unwrap_or_default()
    {
        TestProtocol::Http => "http",
        TestProtocol::Https => "https",
    };

    for service_port_mapping in &resource.spec.ports {
        let health_path = resource.spec.health_endpoint.clone().unwrap_or_default();
        let health_path = health_path.trim_start_matches('/');

        let port = service_port_mapping.proxy_port;

        let test_url =
            format!("{proto}://{test_proxy_service_name}.{namespace}.svc.cluster.local:{port}/{health_path}");

        info!("Testing url {test_url}");

        let http_client = match resource.spec.ignore_certificate_errors.unwrap_or_default() {
            true => &context.danger_ignore_certs_http_client,
            false => &context.http_client,
        };

        let r = http_client.get(test_url).send().await;

        match r {
            Ok(response) => {
                if response.status().is_success() {
                } else {
                    return Err(anyhow!(
                        "Proxied service did respond, but the result was not a success",
                    ));
                }
            }
            Err(e) => return Err(anyhow!("Failed to test proxy service: {}", e)),
        }
    }

    Ok(())
}

async fn create_copy_of_original_service(
    new_name: &str,
    service: &Service,
    client: Client,
) -> Result<(), Error> {
    let mut backup_service = service.clone();
    backup_service.status = None;
    backup_service.metadata = ObjectMeta {
        annotations: backup_service.metadata.annotations.clone(),
        creation_timestamp: None,
        deletion_grace_period_seconds: None,
        deletion_timestamp: None,
        finalizers: None,
        generate_name: None,
        generation: None,
        labels: backup_service.metadata.labels.clone(),
        managed_fields: None,
        name: Some(new_name.to_string()),
        namespace: backup_service.metadata.namespace.clone(),
        owner_references: backup_service.metadata.owner_references.clone(),
        resource_version: None,
        self_link: None,
        uid: None,
    };
    if let Some(ref mut spec) = &mut backup_service.spec {
        spec.cluster_ip = None;
        spec.cluster_ips = None;
    }

    do_server_side_apply(client, backup_service).await?;

    Ok(())
}

async fn restore_proxied_service(
    service_to_replace_name: &str,
    namespace: &str,
    client: Client,
) -> Result<()> {
    info!("Restoring proxies service {service_to_replace_name} in namespace {namespace}");
    let backup_service_name = format!("{}-proxy-backup", service_to_replace_name);

    let original: Service = Api::namespaced(client.clone(), namespace)
        .get(&backup_service_name)
        .await?;

    create_copy_of_original_service(service_to_replace_name, &original, client.clone()).await?;

    Ok(())
}

async fn generate_port_map(
    service_ports: Vec<i32>,
    resource: ReplacedService,
    resource_api: Api<ReplacedService>,
) -> Result<HashMap<i32, i32>, Result<(Action, bool)>> {
    let mut port_map = HashMap::<i32, i32>::new();

    if service_ports.len() > 1 {
        for service_port in service_ports {
            let proxy_port = resource
                .spec
                .ports
                .iter()
                .find(|p| p.original_port == Some(service_port))
                .map(|p| p.proxy_port);

            if let Some(proxy_port) = proxy_port {
                info!(
                    "Found proxy port {} for service port {}",
                    proxy_port, service_port
                );
                port_map.insert(service_port, proxy_port);
            } else {
                let warning = format!(
                    "Service has multiple ports, missing proxy port defined for {}",
                    service_port
                );
                return Err(add_warning_and_requeue(resource, resource_api, &warning).await);
            }
        }
    } else {
        let service_port = service_ports[0];

        match resource.spec.ports.len() {
            0 => return Err(add_warning_and_requeue(resource, resource_api, "Service has no ports defined").await),
            1 => {
                let proxy_port = resource.spec.ports[0].proxy_port;
                info!("Found proxy port {} for service port {}", proxy_port, service_port);
                port_map.insert(service_port, proxy_port);
            },
            _ => return Err(add_warning_and_requeue(resource, resource_api, "ReplacedService has multiple ports, while the service itself only has 1 port defined").await)
        }
    }
    Ok(port_map)
}

async fn add_warning_and_requeue(
    resource: ReplacedService,
    api_client: Api<ReplacedService>,
    message: &str,
) -> Result<(Action, bool)> {
    if let Some(mut latest) = api_client.get_opt(&resource.name_any()).await? {
        let status = latest.status_mut();

        if let Some(ref mut s) = status {
            s.warning = Some(message.to_string());
        } else {
            *status = Some(ReplacedServiceResourceStatus {
                warning: Some(message.to_string()),
                ..Default::default()
            });
        }

        api_client
            .patch_status(
                &latest.name_any(),
                &Default::default(),
                &Patch::Merge(latest),
            )
            .await?;
    }

    Ok((Action::requeue(Duration::from_secs(30)), false))
}

async fn remove_warning_and_requeue(
    resource: ReplacedService,
    api_client: Api<ReplacedService>,
) -> Result<(Action, bool)> {
    if let Some(mut latest) = api_client.get_opt(&resource.name_any()).await? {
        let status = latest.status_mut();

        if let Some(ref mut s) = status {
            s.warning = None;

            api_client
                .patch_status(
                    &latest.name_any(),
                    &Default::default(),
                    &Patch::Merge(latest),
                )
                .await?;
        }
    }

    Ok((Action::requeue(Duration::from_secs(30)), true))
}

async fn do_server_side_apply<TResource>(
    client: Client,
    mut resource: TResource,
) -> Result<TResource>
where
    TResource:
        Resource<Scope = NamespaceResourceScope> + Clone + DeserializeOwned + Serialize + Debug,
    <TResource as Resource>::DynamicType: Default,
{
    info!(
        "Doing server side apply of resource type {}",
        TResource::kind(&Default::default())
    );

    resource.meta_mut().managed_fields = None;

    let api = Api::namespaced(client, &resource.namespace().expect("Expected a namespace"));

    let name = resource.name_unchecked();
    let serverside = PatchParams::apply("tsproxy-operator").force();
    let updated = api
        .patch(&name, &serverside, &Patch::Apply(resource))
        .await?;

    info!(
        "Completed server side apply of resource type {}",
        TResource::kind(&Default::default())
    );
    Ok(updated)
}

async fn create_resource_with_owner_reference<TResource>(
    client: Client,
    namespace: &str,
    name: &str,
    owner_ref: OwnerReference,
    create: impl FnOnce() -> TResource,
) -> Result<TResource>
where
    TResource:
        Resource<Scope = NamespaceResourceScope> + Clone + DeserializeOwned + Serialize + Debug,
    <TResource as Resource>::DynamicType: Default,
{
    info!(
        "Creating resource {name} with owner reference of type {}",
        TResource::kind(&Default::default())
    );
    let api: Api<TResource> = Api::namespaced(client.clone(), namespace);

    if let Some(mut existing) = api.get_opt(name).await? {
        let all_owners = existing.owner_references_mut();

        let found = all_owners.iter().any(|o| o.uid == owner_ref.uid);

        if found {
            Ok(existing)
        } else {
            all_owners.push(owner_ref);

            do_server_side_apply(client, existing).await
        }
    } else {
        let mut resource = create();
        resource.meta_mut().name = Some(name.to_string());
        resource.meta_mut().namespace = Some(namespace.to_string());

        do_server_side_apply(client, resource).await
    }
}

async fn change_deployment_scale(
    client: Client,
    namespace: &str,
    name: &str,
    replicas: i32,
) -> Result<()> {
    info!("Changing deployment scale of {name} in {namespace} to {replicas}");

    let api: Api<Deployment> = Api::namespaced(client.clone(), namespace);

    if let Some(mut deployment) = api.get_opt(name).await? {
        if let Some(ref mut spec) = deployment.spec {
            spec.replicas = Some(replicas);
        }

        do_server_side_apply(client, deployment).await?;
    }

    Ok(())
}
