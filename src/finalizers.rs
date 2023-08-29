use anyhow::anyhow;
use anyhow::Result;
use k8s_openapi::NamespaceResourceScope;
use kube::api::{Patch, PatchParams};
use kube::{Api, Client, Resource, ResourceExt};
use serde::de::DeserializeOwned;
use serde_json::json;
use std::fmt::Debug;

pub const FINALIZER: &str = "tsproxy.digizuite.com/finalizer";

pub async fn add_finalizer<TResource>(
    client: Client,
    name: &str,
    namespace: &str,
) -> Result<TResource>
where
    TResource: Resource<Scope = NamespaceResourceScope> + Clone + DeserializeOwned + Debug,
    <TResource as Resource>::DynamicType: Default,
{
    debug!("Adding finalizer to {namespace}/{name}");
    let api = Api::namespaced(client, namespace);

    let finalizer = json!({
        "metadata": {
            "finalizers": [FINALIZER]
        }
    });

    let patch = Patch::Merge(&finalizer);
    Ok(api.patch(name, &PatchParams::default(), &patch).await?)
}

pub async fn ensure_finalizer<TResource>(resource: TResource, client: Client) -> Result<TResource>
where
    TResource: Resource<Scope = NamespaceResourceScope> + Clone + DeserializeOwned + Debug,
    <TResource as Resource>::DynamicType: Default,
{
    let name = resource.name_any();

    let namespace = resource.namespace().ok_or_else(|| {
        anyhow!(
            "Expected {} resource to be namespaced. Can't deploy to unknown namespace.",
            TResource::kind(&TResource::DynamicType::default())
        )
    })?;

    if resource
        .meta()
        .finalizers
        .as_ref()
        .map_or(true, |finalizers| finalizers.is_empty())
    {
        debug!("Finalizer not found on resource {namespace}/{name}, adding");
        let resource = add_finalizer::<TResource>(client, &name, &namespace).await?;
        debug!("Finalizer added to {namespace}/{name}");
        Ok(resource)
    } else {
        debug!("Finalizer found on resource {namespace}/{name}");
        Ok(resource)
    }
}

pub async fn remove_finalizer<TResource>(
    client: Client,
    name: &str,
    namespace: &str,
) -> Result<TResource>
where
    TResource: Resource<Scope = NamespaceResourceScope> + Clone + DeserializeOwned + Debug,
    <TResource as Resource>::DynamicType: Default,
{
    debug!("Deleting finalizer from {namespace}/{name}");
    let api = Api::namespaced(client, namespace);
    let finalizer = json!({
        "metadata": {
            "finalizers": null
        }
    });

    let patch = Patch::Merge(&finalizer);
    Ok(api.patch(name, &PatchParams::default(), &patch).await?)
}
