# EKS Cluster Upgrade: 1.23 -> 1.24

|                            |                           Value                           |
| :------------------------- | :-------------------------------------------------------: |
| Current version            |                 `v1.23`                  |
| Target version             |                  `v1.24`                  |
| EKS Managed node group(s)  |  ➖   |
| Self-Managed node group(s) |  ✅  |
| Fargate profile(s)         |      ➖      |
| AMI                        |     Amazon      |

### Table of Contents

- [Caveats](#caveats)
- [References](#references)
- [Pre-Upgrade](#pre-upgrade)
- [Upgrade](#upgrade)
  - [Upgrade the Control Plane](#upgrade-the-control-plane)
  - [Upgrade the Data Plane](#upgrade-the-data-plane)
    - [Self-Managed Node Group](#eks-managed-node-group)
  - [Upgrade Addons](#upgrade-addons)
- [Post-Upgrade](#post-upgrade)

## Caveats

- Unless specifically stated, the phrase `Amazon EKS cluster` or just `cluster` throughout this document typically refers to the control plane.
- In-place cluster upgrades can only be upgraded to the next incremental minor version. For example, you can upgrade from Kubernetes version 1.20 to 1.21, but not from 1.20 to 1.22.
- Reverting an upgrade, or downgrading the Kubernetes version of a cluster, is not supported. If you upgrade your cluster to a new Kubernetes version and then want to revert to the previous version, you must create a new cluster and migrate your workloads.
- If the Amazon EKS cluster primary security group has been deleted, the only course of action to upgrade is to create a new cluster and migrate your workloads.
    - The following should return the details of the cluster primary security group. If not, the security group may no longer exist:

        ```sh
        aws ec2 describe-security-groups --group-ids $(aws eks describe-cluster --name <CLUSTER_NAME> \
            --query 'cluster.resourcesVpcConfig.clusterSecurityGroupId' --output text)
        ```

## References

Before upgrading, review the following resources for affected changes in the next version of Kubernetes:

- ℹ️ [Kubernetes `1.24` release announcement](https://kubernetes.io/blog/2022/05/03/kubernetes-1-24-release-announcement/)
- ℹ️ [EKS `1.24` release notes](https://docs.aws.amazon.com/eks/latest/userguide/kubernetes-versions.html#kubernetes-1.24)

## Pre-Upgrade

1. Before updating your control plane to a new Kubernetes version, ensure that the Kubernetes minor version of data plane nodes are the same as your control plane's version.

    Control plane Kubernetes version:
    ```sh
    kubectl version --short
    ```

    Nodes Kubernetes version:
    ```sh
    kubectl get nodes
    ```

    <details>
    <summary>📌 Example</summary>

    Control plane Kubernetes version:

    ```sh
    kubectl version --short

    # Output
    Server Version: v1.21.14-eks-fb459a0
    ```

    Nodes Kubernetes version:

    ```sh
    NAME                        STATUS   ROLES    AGE     VERSION
    ip-10-0-4-19.ec2.internal   Ready    <none>   4h22m   v1.20.15-eks-abc64b1 ❌ # Needs to be upgraded first
    ip-10-0-4-23.ec2.internal   Ready    <none>   4h24m   v1.21.14-eks-fb459a0 ✅ # Ready to upgrade
    ```

    </details>

2. Verify that there are at least 5 free IPs in the VPC subnets used by the control plane. Amazon EKS creates new elastic network interfaces (ENIs) in any of the subnets specified for the control plane. If there are not enough available IPs, then the upgrade will fail (your control plane will stay on the prior version).

    ```sh
    aws ec2 describe-subnets --subnet-ids $(aws eks describe-cluster --name <CLUSTER_NAME> \
      --query 'cluster.resourcesVpcConfig.subnetIds' --output text) --query 'Subnets[*].AvailableIpAddressCount'
    ```

3. Ensure the security groups allow the necessary cluster communication. The new control plane network interfaces may be created in different subnets than what your existing control plane network interfaces are in, so make sure that your security group rules allow the [required cluster communication](https://docs.aws.amazon.com/eks/latest/userguide/sec-group-reqs.html) for any of the subnets that you specified when you created your cluster.

4. Check Kubernetes API version prerequisites and ensure any removed APIs in the next version are updated prior to upgrading the cluster. There are several open source tools that can help you identify deprecated API versions in your Kubernetes manifests. The following open source projects support scanning both your cluster as well as manifest files to identify deprecated and/or removed API versions:

    - https://github.com/FairwindsOps/pluto
    - https://github.com/doitintl/kube-no-trouble
    - https://github.com/rikatz/kubepug

5. Ensure workloads and applications running on the cluster are setup for high-availability to minimize and avoid disruption during the upgrade process.

    - For stateless workloads
        - Specify multiple replicas for your [replica set(s)](https://kubernetes.io/docs/concepts/workloads/controllers/replicaset/)
        - Specify [pod disruption budget](https://kubernetes.io/docs/tasks/run-application/configure-pdb/) for replica sets
    -

## Upgrade

The order of operations to upgrade an Amazon EKS cluster can be summarized as:

- [Upgrade the control plane](#upgrade-the-control-plane)
- [Upgrade the data plane](#upgrade-the-data-plane)
- [Upgrade addons](#upgrade-addons)

### Upgrade the Control Plane

When upgrading the control plane, Amazon EKS performs standard infrastructure and readiness health checks for network traffic on the new control plane nodes to verify that they're working as expected. If any of these checks fail, Amazon EKS reverts the infrastructure deployment, and your cluster control plane remains on the prior Kubernetes version. Running applications aren't affected, and your cluster is never left in a non-deterministic or unrecoverable state. Amazon EKS regularly backs up all managed clusters, and mechanisms exist to recover clusters if necessary.

- ℹ️ [Updating an Amazon EKS cluster Kubernetes version](https://docs.aws.amazon.com/eks/latest/userguide/update-cluster.html)

1. Upgrade the control plane to the next Kubernetes minor version:

    ```sh
    aws eks update-cluster-version --name <CLUSTER_NAME> --kubernetes-version 1.24
    ```

2. Wait for the control plane to finish upgrading before proceeding with any further modifications. The cluster status will change to `ACTIVE` once the upgrade is complete.

    ```sh
    aws eks describe-cluster --name <CLUSTER_NAME> --query 'cluster.status'
    ```

### Upgrade the Data Plane

#### Self-Managed Node Group

- ℹ️ [Self-managed node updates](https://docs.aws.amazon.com/eks/latest/userguide/update-workers.html)

##### Before Upgrading

- It is recommended to use the [instance refresh](https://docs.aws.amazon.com/autoscaling/ec2/userguide/asg-instance-refresh.html) functionality provided by AWS Auto Scaling groups in coordination with the [`node-termination-handler`](https://github.com/aws/aws-node-termination-handler) to gracefully migrate pods from instances scheduled for replacement when upgrading. Once the launch template has been updated with the new AMI ID, the Auto Scaling group will initiate the instance refresh cycle to rollout the replacement of instances to meet the new launch template specification. The `node-termination-handler` listens to the Auto Scaling group lifecycle events to intervene and gracefully migrate pods off of the instance(s) being replaced.

- A recommended starting point for the instance refresh configuration is to use a value of 70% as the minimum healthy percentage and adjust as necessary. Lowering this value will allow more instances to be refreshed at once, however, it will also increase the risk of overwhelming the control plane with requests. Users should aim to replace no more than 100 instances at a time to match the behavior of EKS managed node groups and avoid overwhelming the control plane during an upgrade.

##### Upgrade

1. Update the launch template, specifying the ID of an AMI that matches the control plane's Kubernetes version:

    ```sh
    aws ec2 create-launch-template-version --launch-template-id <LAUNCH_TEMPLATE_ID> \
      --source-version <LAUNCH_TEMPLATE_VERSION> --launch-template-data 'ImageId=<AMI_ID>'
    ```

    You can [retrieve the recommended EKS optimized AL2 AMI ID](https://docs.aws.amazon.com/eks/latest/userguide/retrieve-ami-id.html) by running the following command:

    ```sh
    aws ssm get-parameter --name /aws/service/eks/optimized-ami/1.24/amazon-linux-2/recommended/image_id --query 'Parameter.Value' --output text
    ```

2. Update the autoscaling-group to use the new launch template

    ```sh
    aws autoscaling update-auto-scaling-group --auto-scaling-group-name <ASG_NAME> \
      --launch-template LaunchTemplateId=<LAUNCH_TEMPLATE_ID>,Version='$Latest'
    ```

3. Wait for the instance refresh to complete. From the [documentation](https://docs.aws.amazon.com/autoscaling/ec2/userguide/asg-instance-refresh.html#instance-refresh-how-it-works), here is what happens during the instance refresh:

    > Amazon EC2 Auto Scaling starts performing a rolling replacement of the instances. It takes a set of instances out of service, terminates them, and launches a set of instances with the new desired configuration. Then, it waits until the instances pass your health checks and complete warmup before it moves on to replacing other instances.
    >
    > After a certain percentage of the group is replaced, a checkpoint is reached. Whenever there is a checkpoint, Amazon EC2 Auto Scaling temporarily stops replacing instances, sends a notification, and waits for the amount of time you specified before continuing. After you receive the notification, you can verify that your new instances are working as expected.
    >
    > After the instance refresh succeeds, the Auto Scaling group settings are automatically updated with the configuration that you specified at the start of the operation.

### Upgrade Addons

## Post Upgrade

- ⚠️ Update applications running on the cluster
- ⚠️ Update tools that interact with the cluster (kubectl, awscli, etc.)
- ⚠️ TODO