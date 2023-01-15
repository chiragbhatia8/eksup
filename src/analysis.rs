use std::collections::HashSet;

use aws_sdk_autoscaling::model::AutoScalingGroup;
use aws_sdk_ec2::Client as Ec2Client;
use aws_sdk_eks::{
  model::{Cluster, FargateProfile, Nodegroup, NodegroupIssueCode},
  Client as EksClient,
};
use k8s_openapi::api::core::v1::Node;
use kube::Client as K8s_Client;
use serde::{Deserialize, Serialize};

use crate::{aws, k8s, version};

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Results {
  pub(crate) version_skew: Vec<NodeDetail>,
  pub(crate) control_plane_subnets: Vec<Subnet>,
  pub(crate) node_subnets: Vec<Subnet>,
  pub(crate) pod_subnets: Vec<Subnet>,
  pub(crate) eks_managed_node_group_health: Vec<NodegroupHealthIssue>,
  pub(crate) cluster_health: Vec<ClusterHealthIssue>,
  pub(crate) addon_version_compatibility: Vec<AddonDetail>,
}

pub(crate) async fn execute(
  aws_shared_config: &aws_config::SdkConfig,
  cluster: &Cluster,
) -> Result<Results, anyhow::Error> {
  // Construct clients once
  let asg_client = aws_sdk_autoscaling::Client::new(aws_shared_config);
  let ec2_client = aws_sdk_ec2::Client::new(aws_shared_config);
  let eks_client = aws_sdk_eks::Client::new(aws_shared_config);
  let k8s_client = kube::Client::try_default().await?;

  let cluster_name = cluster.name.as_ref().unwrap();
  let cluster_version = cluster.version.as_ref().unwrap();
  let nodes = k8s::get_nodes(&k8s_client).await?;

  // Get data plane components once
  let eks_managed_nodegroups = aws::get_eks_managed_nodegroups(&eks_client, cluster_name).await?;
  let self_managed_nodegroups = aws::get_self_managed_nodegroups(&asg_client, cluster_name).await?;
  let fargate_profiles = aws::get_fargate_profiles(&eks_client, cluster_name).await?;

  // Checks
  let version_skew = version_skew(cluster_version, &nodes).await?;
  let control_plane_subnets = control_plane_subnets(cluster, &ec2_client).await?;
  let node_subnets = node_subnets(
    &ec2_client,
    eks_managed_nodegroups.clone(),
    fargate_profiles.clone(),
    self_managed_nodegroups.clone(),
  )
  .await?;
  let pod_subnets = pod_subnets(&ec2_client, &k8s_client).await?;

  let eks_managed_node_group_health = eks_managed_node_group_health(eks_managed_nodegroups).await?;
  let cluster_health = cluster_health(cluster).await?;

  let addon_version_compatibility =
    addon_version_compatibility(&eks_client, cluster_name, cluster_version).await?;

  Ok(Results {
    version_skew,
    control_plane_subnets,
    node_subnets,
    pod_subnets,
    eks_managed_node_group_health,
    cluster_health,
    addon_version_compatibility,
  })
}

/// Node details as viewed from the Kubernetes API
///
/// Contains information related to the Kubernetes component versions
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct NodeDetail {
  name: String,
  container_runtime: String,
  kernel_version: String,
  kube_proxy_version: String,
  kublet_version: String,
  kubernetes_version: String,
  control_plane_version: String,
}

/// Check if there are any nodes that are not at the same minor version as the control plane
///
/// Report on the nodes that do not match the same minor version as the control plane
/// so that users can remediate before upgrading.
async fn version_skew(
  control_plane_version: &str,
  nodes: &[Node],
) -> Result<Vec<NodeDetail>, anyhow::Error> {
  let control_plane_minor_version = version::parse_minor(control_plane_version)?;

  let nodes = nodes
    .iter()
    .map(|node| {
      let status = node.status.as_ref().unwrap();
      let node_info = status.node_info.as_ref().unwrap();
      let kubelet_version = node_info.kubelet_version.to_owned();

      NodeDetail {
        name: node.metadata.name.as_ref().unwrap().to_owned(),
        container_runtime: node_info.container_runtime_version.to_owned(),
        kernel_version: node_info.kernel_version.to_owned(),
        kube_proxy_version: node_info.kube_proxy_version.to_owned(),
        kublet_version: kubelet_version.to_owned(),
        kubernetes_version: version::normalize(&kubelet_version).unwrap(),
        control_plane_version: control_plane_version.to_owned(),
      }
    })
    .filter(|node| {
      let node_minor_version = version::parse_minor(&node.kublet_version).unwrap();
      control_plane_minor_version != node_minor_version
    })
    .collect();

  Ok(nodes)
}

/// Subnet details that can affect upgrade behavior
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Subnet {
  id: String,
  availability_zone: String,
  availability_zone_id: String,
  available_ips: i32,
  cidr_block: String,
}

/// Check if the subnets used by the control plane will support an upgrade
///
/// The control plane requires at least 5 free IPs to support an upgrade
/// in order to accommodate the additional set of cross account ENIs created
/// during an upgrade
async fn control_plane_subnets(
  cluster: &Cluster,
  client: &aws_sdk_ec2::Client,
) -> Result<Vec<Subnet>, anyhow::Error> {
  let subnet_ids = cluster
    .resources_vpc_config()
    .unwrap()
    .subnet_ids
    .as_ref()
    .unwrap();

  let subnet_details = aws::get_subnets(client, subnet_ids.clone()).await?;

  let subnets = subnet_details
    .iter()
    .map(|subnet| {
      let id = subnet.subnet_id.as_ref().unwrap();

      Subnet {
        id: id.to_owned(),
        availability_zone: subnet.availability_zone.as_ref().unwrap().to_owned(),
        availability_zone_id: subnet.availability_zone_id.as_ref().unwrap().to_owned(),
        available_ips: subnet.available_ip_address_count.unwrap(),
        cidr_block: subnet.cidr_block.as_ref().unwrap().to_owned(),
      }
    })
    .collect();

  Ok(subnets)
}

/// Check if the subnets used by the data plane nodes will support an upgrade
///
/// This is more of a cautionary "you should be aware" type check to give
/// users the information to understand how their upgrade may be affected
/// if running in an IP constrained network. For example, they may need to
/// reduce the amount of nodes that are being updated at any point of time
/// to reduce the number of IPs being consumed - this also means that it will
/// take more time to update the nodes in the data plane.
///
/// There is a separate check for pods specifically for the scenario where
/// custom networking is used and the pods are consuming IPs from a potentially
/// different set of subnets
async fn node_subnets(
  ec2_client: &Ec2Client,
  eks_managed_nodegroups: Vec<Nodegroup>,
  fargate_profiles: Vec<FargateProfile>,
  self_managed_nodegroups: Vec<AutoScalingGroup>,
) -> Result<Vec<Subnet>, anyhow::Error> {
  // Dedupe subnet IDs that are shared across different compute constructs
  let mut subnet_ids = HashSet::new();

  // EKS managed node group subnets
  for group in eks_managed_nodegroups {
    let subnets = group.subnets.as_ref().unwrap();
    for subnet in subnets {
      subnet_ids.insert(subnet.to_owned());
    }
  }

  // Self managed node group subnets
  for group in self_managed_nodegroups {
    let subnets = group.vpc_zone_identifier.unwrap();
    for subnet in subnets.split(',') {
      subnet_ids.insert(subnet.to_owned());
    }
  }

  // Fargate profile subnets
  for profile in fargate_profiles {
    let subnets = profile.subnets.unwrap();
    for subnet in subnets {
      subnet_ids.insert(subnet.to_owned());
    }
  }

  // Send one query of all the subnets used in the data plane
  let subnet_details = aws::get_subnets(ec2_client, subnet_ids.into_iter().collect()).await?;

  let subnets = subnet_details
    .iter()
    .map(|subnet| {
      let id = subnet.subnet_id.as_ref().unwrap();

      Subnet {
        id: id.to_owned(),
        availability_zone: subnet.availability_zone.as_ref().unwrap().to_owned(),
        availability_zone_id: subnet.availability_zone_id.as_ref().unwrap().to_owned(),
        available_ips: subnet.available_ip_address_count.unwrap(),
        cidr_block: subnet.cidr_block.as_ref().unwrap().to_owned(),
      }
    })
    .collect();

  Ok(subnets)
}

/// Check if the subnets used by the pods will support an upgrade
///
/// This checks for the `ENIConfig` custom resource that is used to configure
/// the AWS VPC CNI for custom networking. The subnet listed for each ENIConfig
/// is queried for its relevant data used to report on the available IPs
async fn pod_subnets(
  ec2_client: &Ec2Client,
  k8s_client: &K8s_Client,
) -> Result<Vec<Subnet>, anyhow::Error> {
  let eniconfigs = k8s::get_eniconfigs(k8s_client).await?;
  let eniconfig_subnets = eniconfigs
    .iter()
    .map(|eniconfig| eniconfig.spec.subnet.as_ref().unwrap().to_owned())
    .collect();

  let subnet_details = aws::get_subnets(ec2_client, eniconfig_subnets).await?;
  let subnets = subnet_details
    .iter()
    .map(|subnet| {
      let id = subnet.subnet_id.as_ref().unwrap();

      Subnet {
        id: id.to_owned(),
        availability_zone: subnet.availability_zone.as_ref().unwrap().to_owned(),
        availability_zone_id: subnet.availability_zone_id.as_ref().unwrap().to_owned(),
        available_ips: subnet.available_ip_address_count.unwrap(),
        cidr_block: subnet.cidr_block.as_ref().unwrap().to_owned(),
      }
    })
    .collect();

  Ok(subnets)
}

/// Nodegroup health issue data
///
/// Nearly similar to the SDK's `NodegroupHealth` but flattened
/// and without `Option()`s to make it a bit more ergonomic here
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct NodegroupHealthIssue {
  name: String,
  code: String,
  message: String,
}

/// Check for any reported health issues on EKS managed node groups
async fn eks_managed_node_group_health(
  nodegroups: Vec<Nodegroup>,
) -> Result<Vec<NodegroupHealthIssue>, anyhow::Error> {
  let health_issues = nodegroups
    .iter()
    .flat_map(|nodegroup| {
      let name = nodegroup.nodegroup_name.as_ref().unwrap();
      let health = nodegroup.health.as_ref().unwrap();
      let issues = health.issues.as_ref().unwrap();

      issues.iter().map(|issue| {
        let code = issue.code().unwrap_or(&NodegroupIssueCode::InternalFailure);
        let message = issue.message().unwrap_or("");

        NodegroupHealthIssue {
          name: name.to_owned(),
          code: code.as_ref().to_string(),
          message: message.to_owned(),
        }
      })
    })
    .collect();

  Ok(health_issues)
}

/// Cluster health issue data
///
/// Nearly identical to the SDK's `ClusterIssue` but allows us to serialize/deserialize
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ClusterHealthIssue {
  code: String,
  message: String,
  resource_ids: Vec<String>,
}

/// Check for any reported health issues on the cluster control plane
async fn cluster_health(cluster: &Cluster) -> Result<Vec<ClusterHealthIssue>, anyhow::Error> {
  let health = cluster.health.as_ref();

  if let Some(health) = health {
    let issues = health
      .issues
      .as_ref()
      .unwrap()
      .to_owned()
      .iter()
      .map(|issue| {
        let code = &issue.code.as_ref().unwrap().to_owned();

        ClusterHealthIssue {
          code: code.as_str().to_string(),
          message: issue.message.as_ref().unwrap().to_string(),
          resource_ids: issue.resource_ids.as_ref().unwrap().to_owned(),
        }
      })
      .collect();

    return Ok(issues);
  };

  Ok(vec![])
}

/// Addon health issue data
///
/// Nearly identical to the SDK's `AddonIssue` but allows us to serialize/deserialize
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct AddonHealthIssue {
  code: String,
  message: String,
  resource_ids: Vec<String>,
}

/// Details of the addon as viewed from an upgrade perspective
///
/// Contains the associated version information to compare the current version
/// of the addon relative to the current "desired" version, as well as
/// relative to the target Kubernetes version "desired" version. It
/// also contains any potential health issues as reported by the EKS API.
/// The intended goal is to be able to plot a path of what steps a user either
/// needs to take to upgrade the cluster, or should consider taking in terms
/// of a recommendation to update to the latest supported version.
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct AddonDetail {
  name: String,
  /// The current version of the add-on
  version: String,
  /// The default and latest add-on versions for the current Kubernetes version
  current_kubernetes_version: aws::AddonVersion,
  /// The default and latest add-on versions for the target Kubernetes version
  target_kubnernetes_version: aws::AddonVersion,
  /// Add-on health issues
  issues: Vec<AddonHealthIssue>,
}

/// Check for any verson compatibility issues for the EKS addons enabled
///
/// TODO - what course of action to take if users do NOT opt into addons
/// via the API. The "core" addons of VPC CNI, CoreDNS, and kube-proxy are
/// all automatically deployed by EKS, what happens to those during an
/// upgrade if users have not "opted in" to managing via the API? Should we
/// fall back to scanning the "core" addons from the cluster itself if
/// they are not present in the API list response?
async fn addon_version_compatibility(
  client: &EksClient,
  cluster_name: &str,
  cluster_version: &str,
) -> Result<Vec<AddonDetail>, anyhow::Error> {
  let mut addon_versions: Vec<AddonDetail> = Vec::new();

  let target_version = format!("1.{}", version::parse_minor(cluster_version)? + 1);
  let addons = aws::get_addons(client, cluster_name).await?;

  for addon in addons {
    let name = addon.addon_name.as_ref().unwrap();
    let health = addon.health.as_ref();

    let current_kubernetes_version = aws::get_addon_versions(client, name, cluster_version).await?;
    let target_kubnernetes_version = aws::get_addon_versions(client, name, &target_version).await?;

    let issues: Vec<AddonHealthIssue> = match health {
      Some(health) => health
        .issues
        .as_ref()
        .unwrap()
        .to_owned()
        .iter()
        .map(|issue| {
          let code = issue.code.as_ref().unwrap().to_owned();

          AddonHealthIssue {
            code: code.as_str().to_string(),
            message: issue.message.as_ref().unwrap().to_string(),
            resource_ids: issue.resource_ids.as_ref().unwrap().to_owned(),
          }
        })
        .collect(),
      None => vec![],
    };

    addon_versions.push(AddonDetail {
      name: name.to_owned(),
      version: addon.addon_version.as_ref().unwrap().to_owned(),
      current_kubernetes_version,
      target_kubnernetes_version,
      issues,
    })
  }

  Ok(addon_versions)
}

// async fn pending_launch_template_updates() -> Result<Option<Vec<String>>, anyhow::Error> {
//   let mut pending_updates: Vec<String> = Vec::new();

//   let asg_client = aws::asg_client().await?;
//   let asgs = aws::get_asgs(&asg_client).await?;

//   for asg in asgs {
//     if let Some(launch_template) = asg.launch_template {
//       if let Some(launch_template_version) = launch_template.version {
//         if launch_template_version == "$Latest" {
//           pending_updates.push(asg.auto_scaling_group_name.unwrap());
//         }
//       }
//     }
//   }

//   if pending_updates.is_empty() {
//     return Ok(None);
//   }

//   Ok(Some(pending_updates))
// }